//! Remote operation over the in-memory transport: a box engine behind a real daemon
//! socket, `devlanes-tunnel`'s box services on the box end of the pipe, and a
//! client engine that connects, mirrors and syncs. Remote is mirroring only: the
//! box refuses daemon requests from a client.
//!
//! Box and client share this machine, so the box's services hold `127.0.0.1:<port>`
//! and the client mirrors on `[::1]` only; routed hostnames then go through relay
//! ports, which keeps every checked byte on the mux.

#[path = "remote_sync.rs"]
mod large_sync;
#[path = "remote_ssh.rs"]
mod real_ssh;

use std::{
    net::{IpAddr, Ipv6Addr},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use devlanes_daemon::{
    Engine, EngineOptions, Handler, Options, Server, Session,
    integrations::{
        Integrations, RemoteConnector, RemotePrompter, RouteEntry, RouteSink, RouteStatus,
    },
};
use devlanes_ipc::Frame;
use devlanes_proto::{
    Context, Control, LaneSummary, Method, Page, Request, RpcError, remote::RemoteClient,
};
use devlanes_state::Store;
use devlanes_transport::{
    AuthorizedTarget, Backoff, BoxDuplex, Dialer, LoopbackDialer, RpcCallError, ServeOptions,
    SessionOptions, Transport,
    memory::{MemoryTransport, memory_transport},
    serve,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    FakeInfra, FakeProcesses, FakeRunner, Harness, LIMIT, Temp, cx, eventually, repo, temp,
    worktree,
};

const BOX: &str = r#"project: boxapp
services:
  web:
    run: "http hello-from-box"
    hosts: ["web.${{ lane.domain }}"]
    sync:
      - from: ext
        to: "~/ext-out"
  echo:
    run: tcp-echo
  secret:
    run: tcp-echo
    hidden: true
  api:
    run: tcp-echo
    hosts: ["api.example.com"]
"#;

/// Waits used by `eventually`.
const SOON: Duration = Duration::from_secs(15);

/// An engine with fake integrations, its own port pool and optional remote parts.
async fn engine_with(
    home: &Path,
    ports: &str,
    remote: Option<Arc<dyn RemoteConnector>>,
    routes: Option<Arc<dyn RouteSink>>,
) -> Harness {
    let runner = FakeRunner::default();
    let processes = FakeProcesses::default();
    let mut integ = Integrations::with_runner(Arc::new(runner.clone()));
    integ.processes = Arc::new(processes.clone());
    if let Some(remote) = remote {
        integ.remote = remote;
    }
    if let Some(routes) = routes {
        integ.routes = routes;
    }
    std::fs::create_dir_all(home).expect("home");
    let store = Store::open(home.join("state.db")).await.expect("store");
    let mut options = EngineOptions::new(home, home.join("d.sock"));
    options.watch = false;
    options.start_timeout = Duration::from_secs(10);
    options.user.ports.range = ports.into();
    let engine = Engine::new(options, store, integ, CancellationToken::new());
    engine.reconcile().await.expect("reconcile");
    Harness {
        engine,
        runner,
        infra: FakeInfra::default(),
        processes,
        home: home.to_path_buf(),
        next_id: AtomicU64::new(1),
    }
}

/// The box: engine, daemon socket and the tunnel services behind a memory pipe.
struct BoxSide {
    h: Harness,
    token: CancellationToken,
    server: JoinHandle<Result<(), devlanes_daemon::Error>>,
    accept: JoinHandle<()>,
    transport: MemoryTransport,
}

impl BoxSide {
    /// Starts the daemon socket and the tunnel services for the box under `tmp`.
    async fn start(tmp: &Temp, name: &str, ports: &str) -> Self {
        Self::start_with(tmp, name, ports, None).await
    }

    /// [`BoxSide::start`] with `dialer` in place of the loopback dialer.
    async fn start_with(
        tmp: &Temp,
        name: &str,
        ports: &str,
        dialer: Option<Arc<dyn Dialer>>,
    ) -> Self {
        let h = engine_with(&tmp.path.join(name), ports, None, None).await;
        let home = tmp.path.join(format!("{name}-srv"));
        let options = Options {
            home: home.clone(),
            socket: crate::endpoint(&home, "run/d.sock"),
        };
        let paths = devlanes_client::Paths {
            run: home.join("run"),
            socket: options.socket.clone(),
            db: home.join("state.db"),
            logs: home.join("logs"),
            home,
        };
        let server = Server::bind(options).await.expect("bind");
        let token = server.shutdown_token();
        let handler: Arc<dyn Handler> = Arc::new(h.engine.clone());
        let server = tokio::spawn(server.run(handler, || Ok(())));
        let (transport, mut listener) = memory_transport();
        let mut services = devlanes_tunnel::services(
            paths,
            RemoteClient {
                pid: 1,
                ssh: Some("memory".into()),
                since_ms: 0,
            },
        );
        if let Some(dialer) = dialer {
            services.dialer = dialer;
        }
        let accept = tokio::spawn(async move {
            while let Some(stream) = listener.accept().await {
                let services = services.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, services, ServeOptions::new("devlanes test-box")).await;
                });
            }
        });
        Self {
            h,
            token,
            server,
            accept,
            transport,
        }
    }

    /// Stops the box daemon (and with it every unit).
    async fn stop(self) {
        self.accept.abort();
        self.token.cancel();
        let _ = self.server.await;
        self.h.close().await;
    }
}

/// A connector handing out the box's memory transport.
struct MemoryConnector {
    transport: MemoryTransport,
}

impl RemoteConnector for MemoryConnector {
    /// The in-memory pipe to the test box.
    fn transport(
        &self,
        _host: &str,
        _prompts: Arc<dyn RemotePrompter>,
    ) -> Result<Arc<dyn Transport>, RpcError> {
        Ok(Arc::new(self.transport.clone()))
    }

    /// Recorded in `remote_hosts`.
    fn kind(&self) -> &'static str {
        "memory"
    }

    /// `127.0.0.1` belongs to the box's services on this shared machine.
    fn mirror_addresses(&self) -> Vec<IpAddr> {
        vec![Ipv6Addr::LOCALHOST.into()]
    }

    /// Fast retries keep the reconnect checks short.
    fn backoff(&self) -> Backoff {
        Backoff::new(Duration::from_millis(50), Duration::from_millis(200))
    }
}

/// The client's proxy, fed like the composition root feeds it.
struct ProxyRoutes(devlanes_proxy::Proxy);

impl RouteSink for ProxyRoutes {
    /// Converts and swaps the route table.
    fn publish(&self, routes: Vec<RouteEntry>) {
        let routes = routes
            .into_iter()
            .map(|entry| {
                let mut route = devlanes_proxy::Route::new(
                    entry.host,
                    devlanes_proxy::Upstream::Local(entry.port),
                );
                route.fallback = entry.fallback;
                route.state = match entry.status {
                    RouteStatus::Ready => devlanes_proxy::RouteState::Ready,
                    RouteStatus::Idle => devlanes_proxy::RouteState::Idle,
                    RouteStatus::Starting => devlanes_proxy::RouteState::Starting,
                    RouteStatus::Failed { log_tail } => {
                        devlanes_proxy::RouteState::Failed { log_tail }
                    }
                    RouteStatus::Reconnecting => devlanes_proxy::RouteState::Reconnecting,
                };
                route.certificate_names = entry.certificate_names;
                route
            })
            .collect();
        if let Ok(table) = devlanes_proxy::RouteTable::new(routes) {
            self.0.replace_routes(table);
        }
    }
}

/// The client engine plus its plain-HTTP proxy port.
struct ClientSide {
    h: Harness,
    proxy_port: u16,
    proxy_stop: CancellationToken,
}

impl ClientSide {
    /// A client engine whose `connect` reaches `side`'s box.
    async fn start(tmp: &Temp, name: &str, ports: &str, side: &BoxSide) -> Self {
        let proxy = devlanes_proxy::Proxy::new(
            devlanes_proxy::ProxyConfig::default(),
            devlanes_proxy::RouteTable::default(),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("proxy listener");
        let proxy_port = listener.local_addr().expect("proxy addr").port();
        let proxy_stop = CancellationToken::new();
        let serving = proxy.clone();
        let stop = proxy_stop.clone();
        tokio::spawn(async move {
            let _ = serving.serve(listener, stop).await;
        });
        let connector = Arc::new(MemoryConnector {
            transport: side.transport.clone(),
        });
        let h = engine_with(
            &tmp.path.join(name),
            ports,
            Some(connector),
            Some(Arc::new(ProxyRoutes(proxy))),
        )
        .await;
        Self {
            h,
            proxy_port,
            proxy_stop,
        }
    }

    /// `host ls` entry of the box.
    async fn host(&self) -> Value {
        let list = self
            .h
            .call(Method::HostList, json!({}), Context::default())
            .await
            .expect("host ls");
        list["items"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["host"] == "box"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Box ports this client mirrors.
    async fn mirrored(&self) -> Vec<u16> {
        self.host().await["ports"]
            .as_array()
            .map(|ports| {
                ports
                    .iter()
                    .filter_map(|p| p["port"].as_u64())
                    .filter_map(|p| u16::try_from(p).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// State of `host` in the client's `hosts` listing.
    async fn route_state(&self, host: &str) -> Option<String> {
        let hosts = self
            .h
            .call(Method::Hosts, json!({}), Context::default())
            .await
            .ok()?;
        hosts["items"]
            .as_array()?
            .iter()
            .find(|item| item["host"] == host)
            .and_then(|item| item["state"].as_str())
            .map(str::to_owned)
    }

    /// Stops the client engine and proxy.
    async fn stop(self) {
        self.proxy_stop.cancel();
        self.h.close().await;
    }
}

/// True when this machine has an IPv6 loopback to mirror on.
async fn ipv6_loopback() -> bool {
    tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
        .await
        .is_ok()
}

/// Box lanes from `status --all`.
async fn box_lanes(h: &Harness) -> Vec<LaneSummary> {
    let value = h
        .call(Method::Status, json!({"all": true}), Context::default())
        .await
        .expect("box status");
    serde_json::from_value::<Page<LaneSummary>>(value)
        .expect("page")
        .items
}

/// The default port of `unit` in the box lane with `index`.
async fn box_port(h: &Harness, index: u32, unit: &str) -> u16 {
    let lanes = box_lanes(h).await;
    let lane = lanes
        .iter()
        .find(|lane| lane.index == Some(index))
        .unwrap_or_else(|| panic!("no box lane {index}"));
    lane.units
        .iter()
        .find(|u| u.name == unit)
        .and_then(|u| u.ports.get("default").copied())
        .unwrap_or_else(|| panic!("no port for {unit}"))
}

/// Round-trips a line through `[::1]:<port>`.
async fn echo(port: u16) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).await.ok()?;
        stream.write_all(b"ping\n").await.ok()?;
        let mut buffer = [0u8; 5];
        stream.read_exact(&mut buffer).await.ok()?;
        Some(&buffer == b"ping\n")
    };
    tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// One GET through the client's proxy with `Host: host`.
async fn http_get(port: u16, host: &str) -> Option<String> {
    let attempt = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await.ok()?;
        Some(response)
    };
    tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .ok()
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
/// Connect mirrors ports and hostnames (hidden units and foreign hostnames
/// excluded, a busy local port reported), the box refuses daemon requests, a
/// folder syncs into the devlanes-owned root, a lane started mid-session gets
/// listeners, a reconnect keeps listeners and resends no unchanged file, a failed
/// link unmirrors, and disconnect cleans up.
async fn remote_box_end_to_end() {
    if !ipv6_loopback().await {
        eprintln!("skipping remote_box_end_to_end: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT * 2, async {
        let tmp = temp();
        let main = repo(&tmp.path, "boxrepo", BOX);
        std::fs::create_dir_all(main.join("ext/js")).expect("ext");
        std::fs::write(main.join("ext/manifest.json"), b"{\"v\":1}").expect("manifest");
        std::fs::write(main.join("ext/js/app.js"), b"console.log(1)").expect("app");
        let side = BoxSide::start(&tmp, "box", "29000-29499").await;
        side.h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("box start");
        let (web, echo_port, secret) = (
            box_port(&side.h, 0, "web").await,
            box_port(&side.h, 0, "echo").await,
            box_port(&side.h, 0, "secret").await,
        );

        let client = ClientSide::start(&tmp, "client", "28000-28499", &side).await;
        // The sync root is always $DEVLANES_HOME/sync/<host>/<project>/<lane>/<leaf>;
        // a marked destination may be pruned, anything else there is stale.
        let out = client.h.home.join("sync/box/boxapp/boxapp@0/ext");
        std::fs::create_dir_all(&out).expect("dest");
        std::fs::write(out.join(devlanes_sync::OWNER_MARKER), b"").expect("marker");
        std::fs::write(out.join("stale.txt"), b"old").expect("stale");
        // Something on this machine holds the web port on ::1: this listener, or
        // (when the bind fails) whatever already had it.
        let squatter = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, web))
            .await
            .ok();

        // Remote is mirroring only: the box refuses a daemon request sent by a client.
        // (Before `connect`, so the client's link stays the transport's last one.)
        let direct =
            devlanes_transport::Session::connect(&side.transport, SessionOptions::new("probe"))
                .await
                .expect("session");
        let refused = direct
            .request(json!({"method": "daemon.shutdown", "params": {}, "context": {"yes": true}}))
            .await;
        let code = match refused {
            Err(RpcCallError::Remote(error)) => error
                .data
                .and_then(|data| serde_json::from_value::<RpcError>(data).ok())
                .map(|error| error.code),
            other => panic!("the box answered a daemon request: {other:?}"),
        };
        assert_eq!(code.as_deref(), Some("not_supported"));
        direct.close();
        assert!(
            !box_lanes(&side.h).await.is_empty(),
            "the box daemon is still running"
        );

        let connected = client
            .h
            .call(
                Method::Connect,
                json!({"host": "box", "background": true}),
                Context::default(),
            )
            .await
            .expect("connect");
        assert_eq!(connected["state"], "connected", "{connected}");

        // Mirrored ports: visible services only.
        let mirrored = eventually(SOON, || async {
            let ports = client.mirrored().await;
            (ports.contains(&web) && ports.contains(&echo_port)).then_some(ports)
        })
        .await
        .expect("ports mirrored");
        assert!(
            !mirrored.contains(&secret),
            "hidden service mirrored: {mirrored:?}"
        );
        assert!(echo(echo_port).await, "raw TCP through the mux");
        assert!(
            TcpStream::connect((Ipv6Addr::LOCALHOST, secret))
                .await
                .is_err(),
            "hidden port has no listener"
        );

        // HTTP through the local proxy to the box service.
        let response = eventually(SOON, || async {
            http_get(client.proxy_port, "web.boxapp.localhost")
                .await
                .filter(|r| r.contains("hello-from-box"))
        })
        .await;
        assert!(response.is_some(), "proxied request reached the box");

        // A busy local port is reported with a clear message, not mirrored.
        let host = client.host().await;
        let busy = format!("port {web} is in use on this machine by");
        assert!(
            host["warnings"].as_array().is_some_and(|w| w
                .iter()
                .any(|m| m.as_str().is_some_and(|m| m.starts_with(&busy)))),
            "{host}"
        );
        drop(squatter);

        // A box hostname outside .localhost is neither routed nor certified here.
        assert!(
            host["skipped_hosts"]
                .as_array()
                .is_some_and(|h| h.iter().any(|h| h["host"] == "api.example.com")),
            "{host}"
        );
        assert_eq!(client.route_state("api.example.com").await, None);

        // The box lists its connected client.
        let status = side
            .h
            .call(Method::Status, json!({"all": true}), Context::default())
            .await
            .expect("box status");
        assert!(
            status["clients"].as_array().is_some_and(|c| !c.is_empty()),
            "box status lists connected clients: {status}"
        );

        // Sync: what differs from the marked destination, stale files pruned, then
        // changes and deletions. The box's `to:` is ignored.
        let copied = eventually(SOON, || async {
            (std::fs::read(out.join("js/app.js")).ok()? == b"console.log(1)"
                && !out.join("stale.txt").exists())
            .then_some(())
        })
        .await;
        assert!(copied.is_some(), "initial copy and prune");
        assert!(!client.h.home.join("ext-out").exists());
        std::fs::write(main.join("ext/js/app.js"), b"console.log(2)").expect("edit");
        std::fs::remove_file(main.join("ext/manifest.json")).expect("delete");
        let followed = eventually(SOON, || async {
            (std::fs::read(out.join("js/app.js")).ok()? == b"console.log(2)"
                && !out.join("manifest.json").exists())
            .then_some(())
        })
        .await;
        assert!(followed.is_some(), "changes and deletions follow");

        // A lane started mid-session gets listeners.
        let second = tmp.path.join("box-wt2");
        worktree(&main, &second, "feat/two");
        side.h
            .call(Method::LaneList, json!({}), cx(&second))
            .await
            .expect("box lane list");
        side.h
            .call(Method::Start, json!({"units": ["echo"]}), cx(&second))
            .await
            .expect("box start lane 1");
        let late = box_port(&side.h, 1, "echo").await;
        let reached = eventually(SOON, || async { echo(late).await.then_some(()) }).await;
        assert!(reached.is_some(), "new lane's port mirrored live");

        // Reconnect: listeners stay bound, routes say reconnecting, the new snapshot wins.
        side.transport.refuse(false);
        side.transport.last_link().expect("link").sever();
        let down = eventually(SOON, || async {
            (client.host().await["state"] == "reconnecting").then_some(())
        })
        .await;
        assert!(down.is_some(), "link reports reconnecting");
        // The mirror republishes routes after it sees the link go down, a moment
        // after `host.list` does; the link stays down until `accept` below.
        let route = eventually(SOON, || async {
            let state = client.route_state("web.boxapp.localhost").await;
            (state.as_deref() == Some("reconnecting")).then_some(())
        })
        .await;
        assert!(
            route.is_some(),
            "route reads reconnecting while the link is down: {:?}",
            client.route_state("web.boxapp.localhost").await
        );
        assert_eq!(client.host().await["state"], "reconnecting");
        assert!(
            TcpStream::connect((Ipv6Addr::LOCALHOST, echo_port))
                .await
                .is_ok(),
            "listener stays bound while reconnecting"
        );
        side.h
            .call(Method::Stop, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("box stop web");
        side.transport.accept();
        let replaced = eventually(SOON, || async {
            let state = client.route_state("web.boxapp.localhost").await?;
            (client.host().await["state"] == "connected"
                && state != "reconnecting"
                && state != "ready")
                .then_some(state)
        })
        .await;
        assert!(
            replaced.is_some(),
            "snapshot after reconnect replaced web's state"
        );
        assert!(
            eventually(SOON, || async { echo(echo_port).await.then_some(()) })
                .await
                .is_some(),
            "echo works after reconnect"
        );
        // The new subscription sent this machine's manifest: it reaches "watching"
        // without writing a file (the previous one wrote several).
        let resynced = eventually(SOON, || async {
            let host = client.host().await;
            let sync = host["syncs"].as_array()?.first()?.clone();
            (sync["state"] == "watching" && sync["written"] == 0).then_some(sync)
        })
        .await;
        assert!(
            resynced.is_some(),
            "unchanged files were resent after reconnect: {}",
            client.host().await
        );
        assert_eq!(
            std::fs::read(out.join("js/app.js")).ok().as_deref(),
            Some(&b"console.log(2)"[..])
        );

        // A link that fails for good unbinds and unroutes, but stays listed.
        side.transport.refuse(true);
        side.transport.last_link().expect("link").sever();
        let failed = eventually(SOON, || async {
            let host = client.host().await;
            (host["state"] == "failed").then_some(host)
        })
        .await
        .expect("link failed");
        assert_eq!(failed["routes"], json!([]), "{failed}");
        let unbound = eventually(SOON, || async {
            TcpStream::connect((Ipv6Addr::LOCALHOST, echo_port))
                .await
                .is_err()
                .then_some(())
        })
        .await;
        assert!(unbound.is_some(), "listeners close when the link fails");
        assert_eq!(client.route_state("web.boxapp.localhost").await, None);
        side.transport.accept();

        // Disconnect unbinds and unroutes.
        client
            .h
            .call(
                Method::Disconnect,
                json!({"host": "box"}),
                Context::default(),
            )
            .await
            .expect("disconnect");
        assert_eq!(client.route_state("web.boxapp.localhost").await, None);
        let unbound = eventually(SOON, || async {
            TcpStream::connect((Ipv6Addr::LOCALHOST, echo_port))
                .await
                .is_err()
                .then_some(())
        })
        .await;
        assert!(unbound.is_some(), "listeners close on disconnect");
        let stored = client.host().await;
        assert_eq!(
            stored["state"], "disconnected",
            "remote_hosts keeps the box: {stored}"
        );

        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}

/// A box lane with a serving unit and an idle one.
const POOLED_BOX: &str = r#"project: pooledbox
services:
  web:
    run: "http hello-from-box"
  echo:
    run: tcp-echo
"#;

/// The ports of every unit in `project`'s lanes on `h`.
async fn project_ports(h: &Harness, project: &str) -> Vec<u16> {
    box_lanes(h)
        .await
        .iter()
        .filter(|lane| lane.project == project)
        .flat_map(|lane| {
            lane.units
                .iter()
                .flat_map(|unit| unit.ports.values().copied())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
/// Box and client allocate from the same pool. Idle box units are not mirrored,
/// the client's lane 0 keeps clear of every box port, and a local fixed block
/// over a mirrored port wins: the mirror lets go before the local service starts.
async fn shared_pool_keeps_box_and_local_ports_apart() {
    if !ipv6_loopback().await {
        eprintln!("skipping shared_pool_keeps_box_and_local_ports_apart: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT * 2, async {
        let tmp = temp();
        let pool = "27000-27499";
        let box_repo = repo(&tmp.path, "pooledbox", POOLED_BOX);
        let side = BoxSide::start(&tmp, "box", pool).await;
        side.h
            .call(Method::Start, json!({"units": ["echo"]}), cx(&box_repo))
            .await
            .expect("box start echo");
        let (web, echo_port) = (
            box_port(&side.h, 0, "web").await,
            box_port(&side.h, 0, "echo").await,
        );
        let box_ports = project_ports(&side.h, "pooledbox").await;

        let client = ClientSide::start(&tmp, "client", pool, &side).await;
        client
            .h
            .call(
                Method::Connect,
                json!({"host": "box", "background": true}),
                Context::default(),
            )
            .await
            .expect("connect");
        let mirrored = eventually(SOON, || async {
            let ports = client.mirrored().await;
            ports.contains(&echo_port).then_some(ports)
        })
        .await
        .expect("echo mirrored");
        assert!(!mirrored.contains(&web), "idle web mirrored: {mirrored:?}");
        assert!(
            TcpStream::connect((Ipv6Addr::LOCALHOST, web))
                .await
                .is_err(),
            "idle web has no listener"
        );

        // A pool-placed local lane 0 shares no port with box lane 0, although
        // both allocate first-fit from the same pool.
        let local_config = POOLED_BOX.replace("pooledbox", "localapp");
        let local_repo = repo(&tmp.path, "localapp", &local_config);
        client
            .h
            .call(Method::Start, json!({"units": ["web"]}), cx(&local_repo))
            .await
            .expect("local start");
        let local_ports = project_ports(&client.h, "localapp").await;
        assert!(!local_ports.is_empty());
        assert!(
            local_ports.iter().all(|port| !box_ports.contains(port)),
            "local {local_ports:?} overlaps box {box_ports:?}"
        );

        // A local lane whose fixed block covers the mirrored echo port: local wins.
        let fixed_config = format!(
            "project: fixedapp\nlanes:\n  block_size: 1\n  vars:\n    port: {echo_port}\nservices:\n  api:\n    run: sleep\n"
        );
        let fixed_repo = repo(&tmp.path, "fixedapp", &fixed_config);
        client
            .h
            .call(Method::Start, json!({}), cx(&fixed_repo))
            .await
            .expect("local fixed start");
        assert_eq!(project_ports(&client.h, "fixedapp").await, [echo_port]);
        let released = eventually(SOON, || async {
            let host = client.host().await;
            let port = host["ports"]
                .as_array()?
                .iter()
                .find(|port| port["port"] == echo_port)?
                .clone();
            (port["addresses"] == json!([])).then_some(host)
        })
        .await
        .expect("mirror released the local port");
        let local_wins = format!("port {echo_port} is allocated to local lane fixedapp@0");
        assert!(
            released["warnings"].as_array().is_some_and(|w| w
                .iter()
                .any(|m| m.as_str().is_some_and(|m| m.starts_with(&local_wins)))),
            "{released}"
        );
        assert!(
            TcpStream::connect((Ipv6Addr::LOCALHOST, echo_port))
                .await
                .is_err(),
            "the mirror no longer listens on the local lane's port"
        );

        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A box lane whose hostname a local lane already serves is skipped whole, with a
/// warning, and none of its ports are mirrored.
async fn colliding_lane_is_skipped() {
    if !ipv6_loopback().await {
        eprintln!("skipping colliding_lane_is_skipped: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let remote_repo = repo(&tmp.path, "boxrepo", BOX);
        let side = BoxSide::start(&tmp, "box", "29500-29999").await;
        side.h
            .call(Method::Start, json!({}), cx(&remote_repo))
            .await
            .expect("box start");
        let local_repo = repo(&tmp.path, "localrepo", BOX);
        let client = ClientSide::start(&tmp, "client", "28500-28999", &side).await;
        client
            .h
            .call(Method::LaneList, json!({}), cx(&local_repo))
            .await
            .expect("local lane list");
        let (session, mut frames) = Session::local(9_001);
        let request = Request {
            id: 1,
            method: Method::Subscribe,
            params: json!({"follow": true}),
            context: Context::default(),
        };
        let engine = client.h.engine.clone();
        let watching = session.clone();
        let events = tokio::spawn(async move { engine.handle(request, watching).await });
        client
            .h
            .call(
                Method::Connect,
                json!({"host": "box", "background": true}),
                Context::default(),
            )
            .await
            .expect("connect");
        let skipped = eventually(SOON, || async {
            let host = client.host().await;
            host["skipped_lanes"]
                .as_array()
                .filter(|lanes| lanes.iter().any(|l| l["lane"] == "boxapp@0"))
                .map(|_| host.clone())
        })
        .await
        .expect("lane skipped");
        assert_eq!(skipped["ports"], json!([]), "no ports of the skipped lane");
        let warned = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(frame) = frames.recv().await {
                if let Frame::Control(Control::Event(event)) = frame
                    && event.event == "remote.warning"
                    && event.data["code"] == "lane_skipped"
                {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(warned, "skip is announced");
        session.close();
        let _ = events.await;
        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}

/// A box dialer that refuses while `refuse` is set and dials loopback otherwise.
struct SwitchDialer {
    refuse: AtomicBool,
}

#[async_trait::async_trait]
impl Dialer for SwitchDialer {
    /// Fails like a service that stopped listening, or dials it.
    async fn dial(&self, target: &AuthorizedTarget) -> std::io::Result<BoxDuplex> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused by the test",
            ));
        }
        LoopbackDialer.dial(target).await
    }
}

/// Connects to `[::1]:<port>` and waits until the mirror closes the connection.
async fn knock(port: u16) {
    let attempt = async {
        let mut stream = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).await.ok()?;
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).await.ok()
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), attempt).await;
}

#[tokio::test(flavor = "multi_thread")]
/// A connection the box refuses shows as the port's `last_error` in `host.list`
/// and emits one `open_failed` warning however often it repeats; the next
/// connection that gets through clears the error.
async fn refused_opens_are_reported() {
    if !ipv6_loopback().await {
        eprintln!("skipping refused_opens_are_reported: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let remote_repo = repo(&tmp.path, "boxrepo", BOX);
        let dialer = Arc::new(SwitchDialer {
            refuse: AtomicBool::new(false),
        });
        let side = BoxSide::start_with(&tmp, "box", "25000-25499", Some(dialer.clone())).await;
        side.h
            .call(Method::Start, json!({"units": ["echo"]}), cx(&remote_repo))
            .await
            .expect("box start");
        let echo_port = box_port(&side.h, 0, "echo").await;
        let client = ClientSide::start(&tmp, "client", "25500-25999", &side).await;
        let (session, mut frames) = Session::local(9_002);
        let request = Request {
            id: 1,
            method: Method::Subscribe,
            params: json!({"follow": true}),
            context: Context::default(),
        };
        let engine = client.h.engine.clone();
        let watching = session.clone();
        let events = tokio::spawn(async move { engine.handle(request, watching).await });
        client
            .h
            .call(
                Method::Connect,
                json!({"host": "box", "background": true}),
                Context::default(),
            )
            .await
            .expect("connect");
        eventually(SOON, || async {
            client.mirrored().await.contains(&echo_port).then_some(())
        })
        .await
        .expect("echo mirrored");

        dialer.refuse.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            knock(echo_port).await;
        }
        let port_entry = || async {
            client.host().await["ports"]
                .as_array()
                .and_then(|ports| ports.iter().find(|port| port["port"] == echo_port).cloned())
                .unwrap_or(Value::Null)
        };
        let failed = eventually(SOON, || async {
            let port = port_entry().await;
            (port["last_error"]["reason"] == "dial_failed").then_some(port)
        })
        .await
        .expect("the refusal is recorded");
        let message = failed["last_error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("refused by the test"), "{failed}");
        assert!(failed["last_error"]["at_ms"].as_u64().is_some(), "{failed}");

        let mut warnings = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(frame) = frames.recv().await {
                if let Frame::Control(Control::Event(event)) = frame
                    && event.event == "remote.warning"
                    && event.data["code"] == "open_failed"
                {
                    warnings.push(event.data);
                }
            }
        })
        .await;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0]["host"], "box");
        assert_eq!(warnings[0]["port"], echo_port);
        assert_eq!(warnings[0]["lane"], "boxapp@0");
        assert_eq!(warnings[0]["unit"], "echo");
        assert_eq!(warnings[0]["reason"], "dial_failed");

        dialer.refuse.store(false, Ordering::SeqCst);
        assert!(echo(echo_port).await, "the port works again");
        let cleared = eventually(SOON, || async {
            let port = port_entry().await;
            (port["port"] == echo_port && port["last_error"].is_null()).then_some(())
        })
        .await;
        assert!(cleared.is_some(), "a working open clears last_error");

        session.close();
        let _ = events.await;
        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}
