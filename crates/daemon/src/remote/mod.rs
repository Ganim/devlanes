//! Remote operation (plan M14).
//!
//! Client side: `connect` keeps one reconnecting [`Link`] per box in the daemon,
//! mirrors the box's registry ([`mirror`]: same-number loopback listeners and proxy
//! routes), applies its folder syncs ([`sync`]) and forwards `--host` requests over
//! the session. Box side: `status` lists the tunnels serving clients and
//! `sync.status` lists the sync entries a tunnel may stream.

mod mirror;
mod sync;

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use devlanes_proto::{
    AttachResult, Context, Event, Method, Outcome, Request, RpcError,
    remote::{self as wire, Forward, Input, RemoteClient, Reply, SyncEntry},
};
use devlanes_transport::{
    ClientRegistry, Link, LinkState, RpcCallError, RpcReceiver, RpcSender, Session as BoxSession,
    SessionOptions,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    Session,
    engine::{Inner, required},
    integrations::{RemotePrompt, RemotePrompter, RouteEntry},
    model::lock,
    registry::internal,
};

/// How long a forwarded request waits for a reconnecting box before failing.
const SESSION_WAIT: Duration = Duration::from_secs(10);
/// `connect --background` returns once connected, failed, or after this long.
const CONNECT_WAIT: Duration = Duration::from_secs(150);
/// Unanswered ssh prompts are cancelled after this long.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Stream ids for forwarded attaches and prompts start far above the engine's own
/// attach ids, so both can share one client connection.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1 << 40);

/// Remote state owned by the engine.
#[derive(Default)]
pub(crate) struct Remotes {
    hosts: Mutex<BTreeMap<String, Arc<HostConn>>>,
    /// Box side: tunnels currently serving a client, for `status`.
    clients: Mutex<BTreeMap<u64, RemoteClient>>,
    /// Forwarded attaches by local `(session, stream)`.
    streams: Mutex<HashMap<(u64, u64), RemoteStream>>,
    next_client: AtomicU64,
}

/// A forwarded attach: the box's stream id and the input side of its call.
struct RemoteStream {
    remote_id: u64,
    input: mpsc::Sender<Input>,
}

impl Remotes {
    /// Mirrored routes of every connected box, in host order.
    pub(crate) fn routes(&self) -> Vec<RouteEntry> {
        self.all()
            .iter()
            .flat_map(|conn| lock(&conn.view).routes.clone())
            .collect()
    }

    /// The connection to `host`, if `connect` created one.
    fn host(&self, host: &str) -> Option<Arc<HostConn>> {
        lock(&self.hosts).get(host).cloned()
    }

    /// Every connection.
    fn all(&self) -> Vec<Arc<HostConn>> {
        lock(&self.hosts).values().cloned().collect()
    }
}

/// True when a request carries `--host` and must run on that box.
///
/// `connect`, `disconnect` and `host ls` manage the connections themselves, so they
/// always run here.
pub(crate) fn forwards(request: &Request) -> bool {
    request
        .context
        .host
        .as_deref()
        .is_some_and(|h| !h.is_empty())
        && !matches!(
            request.method,
            Method::Connect | Method::Disconnect | Method::HostList
        )
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

/// Sends ssh prompts to the most recently attached `connect` CLI.
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
    /// Sync status by `lane:unit#index`.
    syncs: Mutex<BTreeMap<String, Value>>,
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

    /// State plus what is mirrored, for `connect`, `host ls` and `status`.
    fn status_json(&self) -> Value {
        let mut value = self.state_json();
        let lanes = self.registry.current().lanes().count();
        let view = lock(&self.view).clone();
        let syncs: Vec<Value> = lock(&self.syncs).values().cloned().collect();
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
            map.insert("syncs".into(), json!(syncs));
        }
        value
    }

    /// Stops reconnecting, closes the session and ends every task of this host.
    async fn close(&self) {
        self.cancel.cancel();
        let link = self.link.lock().await.take();
        if let Some(link) = link {
            link.shutdown().await;
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

/// `host_not_connected`: `--host` named a box without a `connect`.
fn not_connected(host: &str) -> RpcError {
    RpcError::new("host_not_connected", json!({"host": host}), 4)
}

/// `remote_failed`: the box or the link failed the request.
fn remote_failed(host: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        "remote_failed",
        json!({"host": host, "message": message.to_string()}),
        4,
    )
}

/// Keeps a box daemon's structured error, else wraps the transport failure.
fn call_error(host: &str, error: RpcCallError) -> RpcError {
    match error {
        RpcCallError::Remote(remote) => remote
            .data
            .and_then(|data| serde_json::from_value::<RpcError>(data).ok())
            .unwrap_or_else(|| remote_failed(host, remote.message)),
        other => remote_failed(host, other),
    }
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

/// Encodes terminal bytes for JSON transport values.
fn encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decodes terminal bytes; malformed input becomes an empty frame (end of stream).
fn decode(data: &str) -> Bytes {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map(Bytes::from)
        .unwrap_or_default()
}

/// The destination of a box sync entry on this machine: the same place under this
/// user's home, or the default `~/devlanes/sync/<project>/<lane>/<leaf>` when the
/// box chose a path outside its home.
fn local_destination(home: &Path, entry: &SyncEntry) -> PathBuf {
    match &entry.home_relative {
        Some(relative) if !relative.as_os_str().is_empty() && relative.is_relative() => {
            home.join(relative)
        }
        _ => home
            .join("devlanes/sync")
            .join(&entry.project)
            .join(entry.lane.replace(['/', '@'], "-"))
            .join(&entry.leaf),
    }
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
        let conn = self.ensure_host(&args.host)?;
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

    /// The live connection to `host`, or a new one; a stopped link is replaced.
    fn ensure_host(self: &Arc<Self>, host: &str) -> Result<Arc<HostConn>, RpcError> {
        let (conn, old) = {
            let mut hosts = lock(&self.remote.hosts);
            if let Some(conn) = hosts.get(host)
                && !conn.ended()
            {
                return Ok(conn.clone());
            }
            let conn = self.open_host(host)?;
            (conn.clone(), hosts.insert(host.to_owned(), conn))
        };
        if let Some(old) = old {
            tokio::spawn(async move { old.close().await });
        }
        Ok(conn)
    }

    /// Starts the link and the host's watcher and mirror tasks.
    fn open_host(self: &Arc<Self>, host: &str) -> Result<Arc<HostConn>, RpcError> {
        let board = Arc::new(PromptBoard::default());
        let prompter = Arc::new(HostPrompter {
            board: board.clone(),
        });
        let transport = self.integ.remote.transport(host, prompter)?;
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
            syncs: Mutex::new(BTreeMap::new()),
        });
        tokio::spawn(watch_link(Arc::downgrade(self), conn.clone()));
        tokio::spawn(mirror::run(Arc::downgrade(self), conn.clone()));
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

    /// `host ls`: boxes known from `remote_hosts` plus live connection state.
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

    /// Runs a `--host` request on the box and relays its events and answer.
    ///
    /// The forwarded context drops cwd, environment and host: they describe this
    /// machine. Dropping the call (the CLI left) cancels it on the box.
    pub(crate) async fn forward(
        self: &Arc<Self>,
        request: Request,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let Request {
            method,
            params,
            context,
            ..
        } = request;
        if method == Method::Resize
            && let Some(result) = self.resize_remote(&params, session).await
        {
            return result;
        }
        let host = context.host.clone().unwrap_or_default();
        let conn = self
            .remote
            .host(&host)
            .ok_or_else(|| not_connected(&host))?;
        let box_session = conn
            .session_within(SESSION_WAIT)
            .await
            .ok_or_else(|| remote_failed(&host, "the box is not connected; it is reconnecting"))?;
        let forward = Forward {
            method,
            params,
            context: Context {
                cwd: PathBuf::new(),
                env: None,
                host: None,
                ..context
            },
        };
        let value = serde_json::to_value(&forward).map_err(|e| internal("encode", e))?;
        let call = box_session
            .call(value)
            .await
            .map_err(|error| remote_failed(&host, error))?;
        let (mut sender, mut receiver) = call.into_split();
        if method != Method::Attach {
            sender
                .finish()
                .await
                .map_err(|error| call_error(&host, error))?;
        }
        let disconnected = session.disconnected();
        loop {
            let item = tokio::select! {
                item = receiver.next() => item,
                () = disconnected.cancelled() => {
                    return Err(RpcError::new("client_disconnected", Value::Null, 4));
                }
            };
            let value = match item {
                Some(Ok(value)) => value,
                Some(Err(error)) => return Err(call_error(&host, error)),
                None => {
                    return Err(remote_failed(
                        &host,
                        "the box ended the request without an answer",
                    ));
                }
            };
            let reply: Reply =
                serde_json::from_value(value).map_err(|error| remote_failed(&host, error))?;
            match reply {
                Reply::Event { event } => {
                    if session.event(event).await.is_err() {
                        return Err(RpcError::new("client_disconnected", Value::Null, 4));
                    }
                }
                // Only attach streams bytes, and only after its answer.
                Reply::Bytes { .. } => {}
                Reply::Response {
                    outcome: Outcome::Error { error },
                } => return Err(error),
                Reply::Response {
                    outcome: Outcome::Ok { data },
                } if method == Method::Attach => {
                    return self.adopt_attach(data, sender, receiver, session);
                }
                Reply::Response {
                    outcome: Outcome::Ok { data },
                } => return Ok(data),
            }
        }
    }

    /// Gives a forwarded attach a local stream id and pumps its bytes both ways.
    fn adopt_attach(
        self: &Arc<Self>,
        data: Value,
        mut sender: RpcSender,
        mut receiver: RpcReceiver,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let mut result: AttachResult =
            serde_json::from_value(data).map_err(|e| internal("decode", e))?;
        let remote_id = result.stream_id;
        let local_id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
        result.stream_id = local_id;
        let (input, mut inputs) = mpsc::channel::<Input>(64);
        lock(&self.remote.streams)
            .insert((session.id(), local_id), RemoteStream { remote_id, input });
        // The input half lives until the stream entry (its only sender) goes away.
        tokio::spawn(async move {
            while let Some(input) = inputs.recv().await {
                let Ok(value) = serde_json::to_value(&input) else {
                    continue;
                };
                if sender.send(value).await.is_err() {
                    break;
                }
            }
        });
        let engine = Arc::downgrade(self);
        let session = session.clone();
        tokio::spawn(async move {
            let output = async {
                loop {
                    let reply = match receiver.next().await {
                        Some(Ok(value)) => serde_json::from_value::<Reply>(value).ok(),
                        _ => None,
                    };
                    match reply {
                        Some(Reply::Bytes { data, .. }) => {
                            let bytes = decode(&data);
                            let end = bytes.is_empty();
                            if session.bytes(local_id, bytes).await.is_err() || end {
                                return;
                            }
                        }
                        Some(Reply::Event { event }) => {
                            let _ = session.event(event).await;
                        }
                        Some(Reply::Response { .. }) => {}
                        None => {
                            let _ = session.bytes(local_id, Bytes::new()).await;
                            return;
                        }
                    }
                }
            };
            let disconnected = session.disconnected();
            tokio::select! {
                () = output => {}
                () = disconnected.cancelled() => {}
            }
            if let Some(engine) = engine.upgrade() {
                lock(&engine.remote.streams).remove(&(session.id(), local_id));
            }
        });
        serde_json::to_value(result).map_err(|e| internal("encode", e))
    }

    /// `resize` of a forwarded attach goes to the box through the attach call.
    async fn resize_remote(
        &self,
        params: &Value,
        session: &Session,
    ) -> Option<Result<Value, RpcError>> {
        let resize: devlanes_proto::ResizeParams = serde_json::from_value(params.clone()).ok()?;
        let (remote_id, input) = lock(&self.remote.streams)
            .get(&(session.id(), resize.stream_id))
            .map(|stream| (stream.remote_id, stream.input.clone()))?;
        let sent = input
            .send(Input::Resize {
                stream_id: remote_id,
                columns: resize.columns,
                rows: resize.rows,
            })
            .await;
        Some(Ok(json!({"applied": sent.is_ok()})))
    }

    /// `resize`: forwarded attaches first, then the engine's own streams.
    pub(crate) async fn remote_resize(
        self: &Arc<Self>,
        value: Value,
        session: &Session,
    ) -> Result<Value, RpcError> {
        match self.resize_remote(&value, session).await {
            Some(result) => result,
            None => self.resize_method(value, session).await,
        }
    }

    /// Routes binary frames of prompts and forwarded attaches; false when the
    /// stream is not remote.
    pub(crate) async fn remote_input(
        &self,
        stream_id: u64,
        data: &Bytes,
        session: &Session,
    ) -> bool {
        if self
            .remote
            .all()
            .iter()
            .any(|conn| conn.board.answer(session.id(), stream_id, data))
        {
            return true;
        }
        let route = lock(&self.remote.streams)
            .get(&(session.id(), stream_id))
            .map(|stream| (stream.remote_id, stream.input.clone()));
        let Some((remote_id, input)) = route else {
            return false;
        };
        let _ = input
            .send(Input::Bytes {
                stream_id: remote_id,
                data: encode(data),
            })
            .await;
        true
    }

    /// `status`, plus the tunnels serving clients (on a box) and the boxes this
    /// machine is connected to.
    pub(crate) async fn remote_status(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
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
    /// stream), plus the state of syncs this machine receives from boxes.
    pub(crate) async fn sync_status_method(self: &Arc<Self>) -> Result<Value, RpcError> {
        let mut items = Vec::new();
        for (project, lane) in self.all_lanes() {
            let (alive, home, reference) = {
                let st = lane.st();
                (
                    st.services_alive(),
                    st.env.as_ref().and_then(|env| env.get("HOME").cloned()),
                    st.reference(&project.name()),
                )
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
                    items.push(SyncEntry {
                        lane: reference.clone(),
                        project: project.name(),
                        unit: service.unit.clone(),
                        index,
                        from: spec.from.clone(),
                        to: spec.to.clone(),
                        home_relative: home
                            .as_ref()
                            .and_then(|home| spec.to.strip_prefix(home).ok())
                            .map(Path::to_path_buf),
                        leaf: spec
                            .from
                            .file_name()
                            .map(|leaf| leaf.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "sync".into()),
                        ignore: spec.ignore.clone(),
                    });
                }
            }
        }
        let remote: Vec<Value> = self
            .remote
            .all()
            .iter()
            .flat_map(|conn| lock(&conn.syncs).values().cloned().collect::<Vec<_>>())
            .collect();
        Ok(json!({
            "items": items,
            "remote": remote,
            "truncated": false,
            "next_cursor": null,
        }))
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

/// Publishes state changes, records successful connects and restarts syncs on
/// every new session.
async fn watch_link(engine: std::sync::Weak<Inner>, conn: Arc<HostConn>) {
    let mut state = conn.state.clone();
    let mut syncing: Option<tokio::task::JoinHandle<()>> = None;
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
