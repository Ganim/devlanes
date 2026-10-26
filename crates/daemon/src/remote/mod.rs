//! Remote operation (plan M14). Remote is mirroring only: a client never runs
//! commands on a box, and nothing in this path carries environments or secrets.
//!
//! Client side: `connect` keeps one reconnecting [`Link`] per box in the daemon,
//! mirrors the box's registry ([`mirror`]: same-number loopback listeners and proxy
//! routes) and applies its folder syncs ([`sync`]) below
//! `$DEVLANES_HOME/sync/<host>/`; every box knows this machine by the id and
//! name in [`identity`]. A box that connected stays wanted in `remote_hosts`
//! until `disconnect`, and a starting daemon connects it again. A reconnect
//! that needs a person (a prompt without an attached CLI, rejected
//! credentials after a first success) waits in `needs_auth` for the next
//! `connect`, keeping listeners and routes.
//!
//! Box side: `remote.registry` ([`feed`]) streams what a tunnel's client
//! mirrors, `status` lists the tunnels serving clients and `sync.status` lists
//! the sync entries a tunnel may stream.

mod feed;
mod identity;
mod mirror;
mod sync;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
use devlanes_transport::{
    BoxDuplex, BoxInfo, ClientRegistry, Link, LinkState, Session as BoxSession, SessionOptions,
    Transport, TransportError,
};
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
    integrations::{RemotePrompt, RemotePrompter, RouteEntry},
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
    /// Box side: tunnels currently serving a client, for `status`, each with
    /// the serial of the [`ClientGuard`] that listed it.
    clients: Mutex<BTreeMap<ClientKey, (u64, RemoteClient)>>,
    next_client: AtomicU64,
    /// What this machine sends every box, read from disk on the first connect.
    identity: tokio::sync::OnceCell<identity::ClientIdentity>,
    /// Hands each new connection its place in connect order ([`HostConn::serial`]).
    next_serial: AtomicU64,
    /// Extra HTTPS ports the proxy serves for boxes whose proxy port differs from
    /// this machine's ([`Inner::reconcile_box_ports`]). The lock also serializes
    /// opening and closing them.
    box_ports: tokio::sync::Mutex<BoxPorts>,
    /// Set while a starting daemon reopens the boxes an earlier one had
    /// connected ([`Inner::restore_remotes`]).
    restoring: AtomicBool,
}

/// How often box HTTPS ports that could not be opened are tried again: nothing
/// tells the daemon when another process frees one.
const BOX_PORT_RETRY: Duration = Duration::from_secs(30);

/// The box HTTPS ports the proxy serves or failed to serve.
#[derive(Default)]
struct BoxPorts {
    open: BTreeSet<u16>,
    /// Ports a box needs that could not be opened, retried every [`BOX_PORT_RETRY`].
    failed: BTreeSet<u16>,
    /// The retry timer, while `failed` is not empty.
    retry: Option<JoinHandle<()>>,
}

/// Why the proxy cannot serve a box's HTTPS port, as `proxy_port_in_use` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoxPortProblem {
    /// `in use by nginx (pid 812)`, or why the bind failed.
    holder: String,
    pid: Option<u32>,
}

/// The `remote.warning` code and `HostConn::warnings` key of a box whose
/// `ports.range` overlaps another connected box's.
const RANGE_OVERLAP: &str = "ports_range_overlap";

impl Remotes {
    /// No box is connected and no tunnel serves a client, so an idle exit
    /// would cut nobody off. A box counts as connected while its link
    /// reconnects or waits for a sign-in, and while a starting daemon restores
    /// it, so a daemon with wanted boxes stays up for them.
    pub(crate) fn is_idle(&self) -> bool {
        lock(&self.hosts).is_empty()
            && lock(&self.clients).is_empty()
            && !self.restoring.load(Ordering::Relaxed)
    }

    /// Mirrored routes of every connected box, in connect order.
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

    /// Every connection, in connect order.
    fn all(&self) -> Vec<Arc<HostConn>> {
        let mut all: Vec<Arc<HostConn>> = lock(&self.hosts).values().cloned().collect();
        all.sort_by_key(|conn| conn.serial);
        all
    }

    /// Makes every mirror but `serial`'s reconcile again, after that box's routes
    /// or bound ports changed or it went away: a later box may now skip or regain
    /// a lane, and an earlier one may bind a port it released.
    fn wake_others(&self, serial: u64) {
        for conn in self.all() {
            if conn.serial != serial {
                conn.wake.notify_one();
            }
        }
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
    /// Set when the transport asked something while no CLI was attached;
    /// [`Attended`] reads and clears it around each attempt.
    unanswered: AtomicBool,
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
        let Some(session) = lock(&self.board.watchers).last().cloned() else {
            // A notice needs no answer; anything else leaves the attempt
            // waiting for a sign-in instead of failing or asking again.
            if prompt.kind != "notify" {
                self.board.unanswered.store(true, Ordering::Relaxed);
            }
            return None;
        };
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

/// The box's transport, as the connector built it, with one more rule: an
/// attempt that asked a prompt while no CLI was attached needs a sign-in
/// ([`TransportError::NeedsAuth`]), whatever the transport made of the missing
/// answer, so the link waits for `remote connect` instead of retrying.
struct Attended {
    inner: Arc<dyn Transport>,
    board: Arc<PromptBoard>,
}

#[async_trait]
impl Transport for Attended {
    /// Connects through the inner transport and reclassifies its failure.
    async fn connect(&self) -> Result<BoxDuplex, TransportError> {
        self.board.unanswered.store(false, Ordering::Relaxed);
        let result = self.inner.connect().await;
        let unanswered = self.board.unanswered.swap(false, Ordering::Relaxed);
        result.map_err(|error| {
            if unanswered {
                error.into_needs_auth()
            } else {
                error
            }
        })
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
    /// [`watch_link`], waited for on close, so a disconnect that clears
    /// `remote_hosts.wanted` comes after its last record of a connection.
    watch_task: Mutex<Option<JoinHandle<()>>>,
    /// Sync status by `lane:unit#index`.
    syncs: Mutex<BTreeMap<String, Value>>,
    /// Destination hashes by sync key, reused while files keep size and mtime.
    hashes: Mutex<HashMap<String, HashCache>>,
    /// Version, linger and port range findings about the box, by code.
    warnings: Mutex<BTreeMap<&'static str, String>>,
    /// Place in connect order: a box with a lower serial connected earlier and
    /// keeps the hostnames a later box also claims.
    serial: u64,
    /// What the box said about itself in its latest handshake, kept while it
    /// reconnects.
    info: Mutex<BoxInfo>,
    /// The `proxy_port_in_use` message while the proxy cannot serve the box's
    /// HTTPS port.
    proxy_port: Mutex<Option<String>>,
}

impl HostConn {
    /// The live session, if connected.
    fn session(&self) -> Option<BoxSession> {
        live(&self.state.borrow())
    }

    /// Waits up to `limit` for a live session; `None` when the link gave up or
    /// waits for a sign-in.
    async fn session_within(&self, limit: Duration) -> Option<BoxSession> {
        let mut state = self.state.clone();
        let wait = async {
            loop {
                let (session, over) = {
                    let current = state.borrow_and_update();
                    (
                        live(&current),
                        matches!(
                            *current,
                            LinkState::Failed { .. }
                                | LinkState::Stopped
                                | LinkState::NeedsAuth { .. }
                        ),
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

    /// Why the link is down, while it retries, waits for a sign-in or after
    /// it failed.
    fn link_error(&self) -> Option<String> {
        match &*self.state.borrow() {
            LinkState::Retrying { error, .. }
            | LinkState::NeedsAuth { error }
            | LinkState::Failed { error } => Some(error.clone()),
            _ => None,
        }
    }

    /// True once the link stopped for good, so `connect` must start a new one.
    fn ended(&self) -> bool {
        matches!(
            *self.state.borrow(),
            LinkState::Failed { .. } | LinkState::Stopped
        )
    }

    /// True while the link waits for a person to sign in again.
    fn needs_auth(&self) -> bool {
        matches!(*self.state.borrow(), LinkState::NeedsAuth { .. })
    }

    /// Makes a link waiting for a sign-in try again, now that a CLI can answer
    /// its prompts. Does nothing otherwise.
    async fn resume(&self) {
        if let Some(link) = self.link.lock().await.as_ref()
            && link.resume()
        {
            tracing::info!(host = %self.name, "retrying a box that needs a sign-in");
        }
    }

    /// The error `connect` answers with for a link that failed for good or
    /// waits for a sign-in; `None` otherwise.
    fn stuck(&self) -> Option<RpcError> {
        match &*self.state.borrow() {
            LinkState::Failed { error } => Some(remote_failed(&self.name, error)),
            LinkState::NeedsAuth { error } => Some(remote_needs_auth(&self.name, error)),
            _ => None,
        }
    }

    /// Resolves when the link stops or needs a sign-in again: `Err` carries a
    /// permanent failure or the sign-in error.
    async fn finished(&self) -> Result<(), RpcError> {
        let mut state = self.state.clone();
        loop {
            if matches!(*state.borrow_and_update(), LinkState::Stopped) {
                return Ok(());
            }
            if let Some(error) = self.stuck() {
                return Err(error);
            }
            if state.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    /// Waits until connected, stopped or waiting for a sign-in, at most `limit`.
    async fn settled(&self, limit: Duration) {
        let mut state = self.state.clone();
        let wait = async {
            loop {
                let done = matches!(
                    *state.borrow_and_update(),
                    LinkState::Connected(_)
                        | LinkState::Failed { .. }
                        | LinkState::NeedsAuth { .. }
                        | LinkState::Stopped
                );
                if done || state.changed().await.is_err() {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(limit, wait).await;
    }

    /// `remote.state` payload for the current state, with the command that
    /// resumes a link waiting for a sign-in as `fix`.
    fn state_json(&self) -> Value {
        let mut value = describe(&self.state.borrow());
        if let Some(map) = value.as_object_mut() {
            map.insert("host".into(), json!(self.name));
            if map.get("state").and_then(Value::as_str) == Some("needs_auth") {
                map.insert("fix".into(), json!(sign_in_command(&self.name)));
            }
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
        warnings.extend(lock(&self.proxy_port).clone());
        warnings.extend(view.warnings());
        let https_port = lock(&self.info).https_port;
        if let Some(map) = value.as_object_mut() {
            map.insert("transport".into(), json!(self.kind));
            map.insert("https_port".into(), json!(https_port));
            map.insert("lanes".into(), json!(lanes));
            map.insert("ports".into(), json!(view.ports_json()));
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
        let watcher = lock(&self.watch_task).take();
        if let Some(watcher) = watcher {
            let _ = watcher.await;
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
        LinkState::NeedsAuth { error } => json!({"state": "needs_auth", "error": error}),
        LinkState::Failed { error } => json!({"state": "failed", "error": error}),
        LinkState::Stopped => json!({"state": "stopped"}),
    }
}

/// The command that signs in to `host` again and resumes its link.
fn sign_in_command(host: &str) -> String {
    format!("devlanes remote connect {host}")
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

/// `remote_needs_auth`: the link waits for a person to sign in again, with
/// the command that does it as `fix`.
fn remote_needs_auth(host: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        "remote_needs_auth",
        json!({"host": host, "message": message.to_string(), "fix": sign_in_command(host)}),
        4,
    )
}

/// `connect` parameters.
#[derive(Debug, Deserialize)]
struct ConnectParams {
    host: String,
    #[serde(default)]
    background: bool,
}

/// `disconnect` parameters; no host means every box.
#[derive(Debug, Default, Deserialize)]
struct DisconnectParams {
    #[serde(default)]
    host: Option<String>,
}

/// Where the box lists a client: under its id, so a reconnect replaces the
/// entry its dropped connection left, or apart when it sent no id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ClientKey {
    Id(String),
    Anonymous(u64),
}

/// Removes a tunnel from the box's client list when its registry feed ends.
struct ClientGuard {
    engine: Arc<Inner>,
    key: ClientKey,
    serial: u64,
}

impl Drop for ClientGuard {
    /// Forgets the tunnel, unless a newer connection of the same client
    /// replaced its entry.
    fn drop(&mut self) {
        let mut clients = lock(&self.engine.remote.clients);
        if clients
            .get(&self.key)
            .is_some_and(|(serial, _)| *serial == self.serial)
        {
            clients.remove(&self.key);
        }
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
    // Windows stores the host, which is what the user typed (`ssh://box:2222`
    // suits ssh), under its encoded file name; Unix keeps it as it is. The
    // encoding would turn `..` into a plain name, so that is refused first.
    #[cfg(windows)]
    let host = match host.as_str() {
        "." | ".." => return Err(format!("the box sent an unusable host name {host:?}")),
        _ => devlanes_core::paths::name_on_disk(&host).into_owned(),
    };
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
        let (conn, _watching) = self.ensure_host(&args.host, Some(session), false).await?;
        // Connecting to a box again retries a box HTTPS port that was taken.
        self.reconcile_box_ports().await;
        // This CLI can answer prompts now, so a link waiting for one goes on.
        conn.resume().await;
        if args.background {
            conn.settled(CONNECT_WAIT).await;
            if let Some(error) = conn.stuck() {
                return Err(error);
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
            finished = conn.finished() => finished.map(|()| conn.status_json()),
        }
    }

    /// The live connection to `host`, or a new one. A stopped link is closed, with
    /// its listeners, before its replacement starts. `watcher`, the CLI of a
    /// `connect`, gets the link's prompts from before its first attempt until
    /// the returned guard drops. `connected_before` marks a box restored after
    /// a daemon restart (see [`RemoteConnector::transport`]).
    ///
    /// [`RemoteConnector::transport`]: crate::integrations::RemoteConnector::transport
    async fn ensure_host(
        self: &Arc<Self>,
        host: &str,
        watcher: Option<&Session>,
        connected_before: bool,
    ) -> Result<(Arc<HostConn>, Option<WatchGuard>), RpcError> {
        let identity = self.client_identity().await?;
        let _connecting = self.remote.connecting.lock().await;
        let old = {
            let mut hosts = lock(&self.remote.hosts);
            match hosts.get(host) {
                Some(conn) if !conn.ended() => {
                    let guard = watcher.map(|session| conn.board.watch(session));
                    return Ok((conn.clone(), guard));
                }
                _ => hosts.remove(host),
            }
        };
        if let Some(old) = old {
            old.close().await;
            self.remote.wake_others(old.serial);
            self.refresh_range_overlaps();
        }
        let (conn, guard) = self.open_host(host, identity, watcher, connected_before)?;
        lock(&self.remote.hosts).insert(host.to_owned(), conn.clone());
        Ok((conn, guard))
    }

    /// The id and name this machine sends to boxes, created on first use.
    /// Failing names the file: `client_identity` with `{message, path}`.
    async fn client_identity(&self) -> Result<identity::ClientIdentity, RpcError> {
        self.remote
            .identity
            .get_or_try_init(|| async {
                let home = self.opts.home.clone();
                let failed = |error: &dyn std::fmt::Display| {
                    RpcError::new(
                        "client_identity",
                        json!({
                            "message": error.to_string(),
                            "path": self.opts.home.join(identity::ID_FILE),
                        }),
                        1,
                    )
                };
                tokio::task::spawn_blocking(move || identity::load(&home))
                    .await
                    .map_err(|error| failed(&error))?
                    .map_err(|error| failed(&error))
            })
            .await
            .cloned()
    }

    /// Starts the link and the host's watcher and mirror tasks, with
    /// `watcher` registered for prompts before the link's first attempt.
    fn open_host(
        self: &Arc<Self>,
        host: &str,
        identity: identity::ClientIdentity,
        watcher: Option<&Session>,
        connected_before: bool,
    ) -> Result<(Arc<HostConn>, Option<WatchGuard>), RpcError> {
        let board = Arc::new(PromptBoard::default());
        let prompter = Arc::new(HostPrompter {
            board: board.clone(),
        });
        let transport = Arc::new(Attended {
            inner: self
                .integ
                .remote
                .transport(host, prompter, connected_before)?,
            board: board.clone(),
        });
        let guard = watcher.map(|session| board.watch(session));
        let mut options = SessionOptions::new(format!("devlanes {}", devlanes_version::VERSION));
        options.client_id = identity.id;
        options.client_name = identity.name;
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
            watch_task: Mutex::new(None),
            syncs: Mutex::new(BTreeMap::new()),
            hashes: Mutex::new(HashMap::new()),
            warnings: Mutex::new(BTreeMap::new()),
            serial: self.remote.next_serial.fetch_add(1, Ordering::Relaxed),
            info: Mutex::new(BoxInfo::default()),
            proxy_port: Mutex::new(None),
        });
        let watcher = tokio::spawn(watch_link(Arc::downgrade(self), conn.clone()));
        *lock(&conn.watch_task) = Some(watcher);
        let mirror = tokio::spawn(mirror::run(Arc::downgrade(self), conn.clone()));
        *lock(&conn.mirror_task) = Some(mirror);
        Ok((conn, guard))
    }

    /// `disconnect [host]`: closes one box or all of them, unmirrors them and
    /// clears `remote_hosts.wanted`, so a restarted daemon leaves them alone.
    /// A wanted box without a connection here, such as one whose restore
    /// failed, is only forgotten.
    pub(crate) async fn disconnect_method(
        self: &Arc<Self>,
        value: Value,
    ) -> Result<Value, RpcError> {
        let args: DisconnectParams = crate::engine::params(value)?;
        let wanted: BTreeSet<String> = match self.store.remote_hosts().await {
            Ok(hosts) => hosts
                .into_iter()
                .filter(|host| host.wanted)
                .map(|host| host.name)
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "cannot read the boxes to restore");
                BTreeSet::new()
            }
        };
        let closing: Vec<Arc<HostConn>> = {
            let mut hosts = lock(&self.remote.hosts);
            match &args.host {
                Some(host) => match hosts.remove(host) {
                    Some(conn) => vec![conn],
                    None if wanted.contains(host) => Vec::new(),
                    None => return Err(not_connected(host)),
                },
                None => std::mem::take(&mut *hosts).into_values().collect(),
            }
        };
        let mut names = BTreeSet::new();
        for conn in closing {
            conn.close().await;
            self.bus.emit(
                wire::STATE_EVENT,
                json!({"host": conn.name, "state": "stopped"}),
            );
            names.insert(conn.name.clone());
            self.remote.wake_others(conn.serial);
        }
        match &args.host {
            Some(host) => {
                names.insert(host.clone());
            }
            None => names.extend(wanted.iter().cloned()),
        }
        // After `close`, which waited for the last record of a connection.
        for name in &names {
            if let Err(error) = self.store.set_remote_host_wanted(name.clone(), false).await {
                tracing::warn!(%error, host = %name, "a restarted daemon may connect this box again");
            }
        }
        let names: Vec<String> = names.into_iter().collect();
        self.refresh_range_overlaps();
        self.reconcile_box_ports().await;
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
        let clients: Vec<RemoteClient> = lock(&self.remote.clients)
            .values()
            .map(|(_, client)| client.clone())
            .collect();
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

    /// Connects again, in the background, every box an earlier daemon had
    /// connected and that was not disconnected since, with no CLI attached: a
    /// prompt then leaves the box waiting for `remote connect`. Until every
    /// such box is open, the daemon does not count as idle.
    pub(crate) fn restore_remotes(self: &Arc<Self>) {
        self.remote.restoring.store(true, Ordering::Relaxed);
        let engine = self.clone();
        tokio::spawn(async move {
            engine.restore_wanted().await;
            engine.remote.restoring.store(false, Ordering::Relaxed);
        });
    }

    /// [`Inner::restore_remotes`]'s work.
    async fn restore_wanted(self: &Arc<Self>) {
        let hosts = match self.store.remote_hosts().await {
            Ok(hosts) => hosts,
            Err(error) => {
                tracing::warn!(%error, "cannot read the boxes to restore");
                return;
            }
        };
        let mut restored = false;
        for host in hosts.into_iter().filter(|host| host.wanted) {
            if self.shutdown.is_cancelled() {
                return;
            }
            match self.ensure_host(&host.name, None, true).await {
                // A shutdown that began meanwhile may have closed the others already.
                Ok((conn, _)) if self.shutdown.is_cancelled() => {
                    lock(&self.remote.hosts).remove(&conn.name);
                    conn.close().await;
                    return;
                }
                Ok(_) => {
                    tracing::info!(host = %host.name, "reconnecting a box connected before the restart");
                    restored = true;
                }
                Err(error) => {
                    tracing::warn!(host = %host.name, code = %error.code, "cannot restore the box connection");
                }
            }
        }
        if restored {
            self.reconcile_box_ports().await;
        }
    }

    /// Closes every box connection; called on daemon shutdown so ssh masters exit.
    pub(crate) async fn remote_shutdown(self: &Arc<Self>) {
        let hosts: Vec<Arc<HostConn>> = std::mem::take(&mut *lock(&self.remote.hosts))
            .into_values()
            .collect();
        for conn in hosts {
            conn.close().await;
        }
        self.reconcile_box_ports().await;
    }

    /// Opens the extra HTTPS port of every box whose proxy port differs from
    /// this machine's, so the URLs the box generated open here unchanged, and
    /// closes the ports no box needs anymore. A box keeps its port while it
    /// reconnects, like its mirrored ports. A port that cannot be opened is
    /// retried on the next call and every [`BOX_PORT_RETRY`], and reported with
    /// `proxy_port_in_use` to each box needing it, again only when the reason
    /// changes. Looking up the holder can be slow, so it runs without the lock.
    pub(super) async fn reconcile_box_ports(self: &Arc<Self>) {
        let own = self.integ.routes.https_port();
        let mut wanted: BTreeMap<u16, Vec<Arc<HostConn>>> = BTreeMap::new();
        for conn in self.remote.all() {
            let port = lock(&conn.info).https_port;
            match (port, own) {
                (Some(port), Some(own)) if port != own && !conn.ended() => {
                    wanted.entry(port).or_default().push(conn);
                }
                _ => *lock(&conn.proxy_port) = None,
            }
        }
        let mut failures = Vec::new();
        {
            let mut ports = self.remote.box_ports.lock().await;
            ports.failed.retain(|port| wanted.contains_key(port));
            let stale: Vec<u16> = ports
                .open
                .iter()
                .copied()
                .filter(|port| !wanted.contains_key(port))
                .collect();
            for port in stale {
                ports.open.remove(&port);
                self.integ.routes.close_box_port(port).await;
            }
            for (port, conns) in &wanted {
                if !ports.open.contains(port) {
                    match self.integ.routes.open_box_port(*port) {
                        Ok(()) => {
                            ports.failed.remove(port);
                            ports.open.insert(*port);
                        }
                        Err(error) => {
                            ports.failed.insert(*port);
                            failures.push((*port, error, conns.clone()));
                            continue;
                        }
                    }
                }
                for conn in conns {
                    *lock(&conn.proxy_port) = None;
                }
            }
            if !ports.failed.is_empty() && ports.retry.is_none() {
                ports.retry = Some(tokio::spawn(retry_box_ports(Arc::downgrade(self))));
            }
        }
        let Some(own) = own else { return };
        for (port, error, conns) in failures {
            let problem = self.box_port_problem(port, &error).await;
            let ports = self.remote.box_ports.lock().await;
            // Opened by another reconcile while the holder was looked up.
            if !ports.failed.contains(&port) {
                continue;
            }
            for conn in &conns {
                let message = format!(
                    "the box's URLs use HTTPS port {port}, which this machine cannot serve: {}; open its hostnames on port {own} instead",
                    problem.holder
                );
                let changed = {
                    let mut current = lock(&conn.proxy_port);
                    let changed = current.as_ref() != Some(&message);
                    *current = Some(message.clone());
                    changed
                };
                if changed {
                    tracing::warn!(host = %conn.name, port, %message, "cannot serve the box's https port");
                    self.bus.emit(
                        wire::WARNING_EVENT,
                        json!({
                            "host": conn.name,
                            "code": "proxy_port_in_use",
                            "port": port,
                            "own_port": own,
                            "holder": problem.holder,
                            "pid": problem.pid,
                            "message": message,
                        }),
                    );
                }
            }
            drop(ports);
        }
    }

    /// What keeps the proxy from serving a box's HTTPS `port`: the process holding
    /// it, or why the bind failed.
    async fn box_port_problem(&self, port: u16, error: &std::io::Error) -> BoxPortProblem {
        if error.kind() != std::io::ErrorKind::AddrInUse {
            return BoxPortProblem {
                holder: mirror::bind_failure(error),
                pid: None,
            };
        }
        let pid = self.integ.ports.holder(port).await;
        let holder = match pid {
            Some(pid) => format!("in use by {} (pid {pid})", mirror::process_name(pid).await),
            None => "in use by another process".to_owned(),
        };
        BoxPortProblem { holder, pid }
    }

    /// Updates the `ports_range_overlap` finding of every live box, after a box
    /// connected, reconnected or went away, and emits it where it appeared or
    /// changed. Only boxes are compared: local allocations already keep clear
    /// of box ports, while two boxes know nothing of each other's.
    fn refresh_range_overlaps(&self) {
        let (live, ended): (Vec<Arc<HostConn>>, Vec<Arc<HostConn>>) = self
            .remote
            .all()
            .into_iter()
            .partition(|conn| !conn.ended());
        for conn in ended {
            lock(&conn.warnings).remove(RANGE_OVERLAP);
        }
        for conn in &live {
            let finding = range_overlap(conn, &live);
            let changed = {
                let mut warnings = lock(&conn.warnings);
                match &finding {
                    Some((message, _)) => {
                        warnings.insert(RANGE_OVERLAP, message.clone()).as_ref() != Some(message)
                    }
                    None => {
                        warnings.remove(RANGE_OVERLAP);
                        false
                    }
                }
            };
            if let (true, Some((message, mut data))) = (changed, finding) {
                tracing::warn!(host = %conn.name, code = RANGE_OVERLAP, %message, "box warning");
                if let Some(fields) = data.as_object_mut() {
                    fields.insert("host".into(), json!(conn.name));
                    fields.insert("code".into(), json!(RANGE_OVERLAP));
                    fields.insert("message".into(), json!(message));
                }
                self.bus.emit(wire::WARNING_EVENT, data);
            }
        }
    }
}

/// Tries the failed box HTTPS ports again every [`BOX_PORT_RETRY`] until none
/// is left or the engine is gone. Boxed because `reconcile_box_ports` spawns it.
fn retry_box_ports(engine: std::sync::Weak<Inner>) -> futures_util::future::BoxFuture<'static, ()> {
    Box::pin(async move {
        loop {
            tokio::time::sleep(BOX_PORT_RETRY).await;
            let Some(inner) = engine.upgrade() else {
                return;
            };
            {
                let mut ports = inner.remote.box_ports.lock().await;
                if ports.failed.is_empty() {
                    // Cleared under the lock, so the next failure starts a new timer.
                    ports.retry = None;
                    return;
                }
            }
            inner.reconcile_box_ports().await;
        }
    })
}

/// The `ports_range_overlap` finding for `conn` among the `live` boxes, as
/// message and event fields: its box's `ports.range` overlaps another box's.
/// Lanes allocated on both can then get the same ports, and only one box's
/// can be mirrored.
fn range_overlap(conn: &HostConn, live: &[Arc<HostConn>]) -> Option<(String, Value)> {
    let range = lock(&conn.info).ports_range.clone()?;
    let theirs = parse_range(&range)?;
    let mut overlaps = Vec::new();
    for other in live {
        if other.serial == conn.serial {
            continue;
        }
        let Some(other_range) = lock(&other.info).ports_range.clone() else {
            continue;
        };
        if parse_range(&other_range).is_some_and(|bounds| ranges_overlap(bounds, theirs)) {
            overlaps.push(format!("box {} ({other_range})", other.name));
        }
    }
    if overlaps.is_empty() {
        return None;
    }
    let message = format!(
        "the box's ports.range {range} overlaps that of {}, so lanes on both can get the same ports and only one box's are mirrored here; give one box a different ports.range",
        overlaps.join(" and ")
    );
    Some((message, json!({"range": range, "overlaps": overlaps})))
}

/// A `ports.range` value (`20000-32767`) as inclusive bounds, `None` when invalid.
fn parse_range(range: &str) -> Option<(u16, u16)> {
    devlanes_config::PortsConfig {
        range: range.trim().to_owned(),
    }
    .bounds()
    .ok()
}

/// Whether two inclusive port ranges share a port.
fn ranges_overlap(a: (u16, u16), b: (u16, u16)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

/// Findings about the box behind a handshake label (`devlanes <version>`,
/// optionally `(linger off)`), as warning code and message.
fn label_warnings(label: &str) -> Vec<(&'static str, String)> {
    let ours = devlanes_version::VERSION;
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
                // A box that connected is restored after a restart until
                // `remote disconnect`.
                wanted: true,
            };
            if let Err(error) = inner.store.upsert_remote_host(record).await {
                tracing::warn!(%error, host = %conn.name, "cannot record remote host");
            }
            *lock(&conn.info) = session.box_info().clone();
            let findings = label_warnings(session.server());
            let known = {
                let mut warnings = lock(&conn.warnings);
                let known = warnings.clone();
                warnings.retain(|code, _| *code == RANGE_OVERLAP);
                warnings.extend(findings.iter().cloned());
                known
            };
            // A reconnect repeats a finding only in `remote ls`, not as an event.
            for (code, message) in findings {
                if known.get(code) == Some(&message) {
                    continue;
                }
                tracing::warn!(host = %conn.name, code, %message, "box warning");
                inner.bus.emit(
                    wire::WARNING_EVENT,
                    json!({"host": conn.name, "code": code, "message": message}),
                );
            }
            inner.refresh_range_overlaps();
            inner.reconcile_box_ports().await;
            // The mirror stamps the box's HTTPS port on its routes.
            conn.wake.notify_one();
            if let Some(task) = syncing.take() {
                task.abort();
            }
            syncing = Some(tokio::spawn(sync::run(
                engine.clone(),
                conn.clone(),
                session,
            )));
        } else if conn.ended() {
            inner.refresh_range_overlaps();
            inner.reconcile_box_ports().await;
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

    /// Windows encodes what the user typed as the host, and refuses box names
    /// it cannot store.
    #[cfg(windows)]
    #[test]
    fn windows_sync_destinations_hold_only_windows_names() {
        let home = Path::new(r"C:\Users\acme\.devlanes");
        assert_eq!(
            sync_destination(home, "ssh://box:2222", &entry("app", "app@1", "ext")),
            Ok(home.join(r"sync\ssh%3A__box%3A2222\app\app@1\ext"))
        );
        for bad in [
            entry("con", "app@1", "ext"),
            entry("app", "app@1", "a:b"),
            entry("app", "app@1", r"a\..\..\x"),
            entry("app", "app@1", "dist."),
        ] {
            assert!(sync_destination(home, "box", &bad).is_err(), "{bad:?}");
        }
        assert!(sync_destination(home, ".", &entry("app", "app@1", "ext")).is_err());
    }

    #[test]
    fn labels_report_version_skew_and_linger() {
        let ours = format!("devlanes {}", devlanes_version::VERSION);
        assert!(label_warnings(&ours).is_empty());
        let codes: Vec<_> = label_warnings("devlanes 0.0.0-other (linger off)")
            .into_iter()
            .map(|(code, _)| code)
            .collect();
        assert_eq!(codes, ["box_version", "box_linger_off"]);
    }

    /// Ranges overlap when they share one port; invalid ranges never parse.
    #[test]
    fn port_ranges_overlap_when_they_share_a_port() {
        let range = |text| parse_range(text).expect(text);
        assert!(ranges_overlap(range("20000-24999"), range("24999-29999")));
        assert!(ranges_overlap(range("20000-32767"), range("25000-26000")));
        assert!(!ranges_overlap(range("20000-24999"), range("25000-29999")));
        assert_eq!(parse_range("0-100"), None);
        assert_eq!(parse_range("300-200"), None);
        assert_eq!(parse_range("any"), None);
    }
}
