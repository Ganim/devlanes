//! Several boxes on one client, and boxes whose proxy serves HTTPS on another
//! port than the client's. A child of `remote`, whose box and client harness it
//! reuses.

use std::{sync::Arc, time::Duration};

use devlanes_daemon::{Handler, Session};
use devlanes_ipc::Frame;
use devlanes_pki::CertificateAuthority;
use devlanes_proto::{Context, Control, Method, Request};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    task::JoinHandle,
};

use super::{BOX, BoxSide, ClientSide, SOON, box_port, http_get, ipv6_loopback};
use crate::{LIMIT, cx, eventually, repo, temp};

/// A project with one service that takes the literal `port` and binds
/// nothing, so it can share its number with another box's unit.
fn fixed_port_project(port: u16) -> String {
    format!(
        "project: fixedbox\nlanes:\n  block_size: 1\n  vars:\n    port: {port}\nservices:\n  api:\n    run: sleep\n"
    )
}

/// Subscribes to the client's events, as an attached CLI would.
fn subscribe(client: &ClientSide, id: u64) -> (Session, mpsc::Receiver<Frame>, JoinHandle<()>) {
    let (session, frames) = Session::local(id);
    let request = Request {
        id: 1,
        method: Method::Subscribe,
        params: json!({"follow": true}),
        context: Context::default(),
    };
    let engine = client.h.engine.clone();
    let watching = session.clone();
    let task = tokio::spawn(async move {
        let _ = engine.handle(request, watching).await;
    });
    (session, frames, task)
}

/// The `remote.warning` events received within a short while.
async fn warnings(frames: &mut mpsc::Receiver<Frame>) -> Vec<Value> {
    let mut found = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(frame) = frames.recv().await {
            if let Frame::Control(Control::Event(event)) = frame
                && event.event == "remote.warning"
            {
                found.push(event.data);
            }
        }
    })
    .await;
    found
}

/// Whether the `remote ls` entry `host` has a warning containing `text`.
fn warns(host: &Value, text: &str) -> bool {
    host["warnings"].as_array().is_some_and(|warnings| {
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|w| w.contains(text)))
    })
}

#[tokio::test(flavor = "multi_thread")]
/// Two boxes running the same project: the box connected later skips its lane
/// with a warning naming the first, a port the first box's mirror holds is
/// reported with that box, and the overlapping `ports.range` is warned about.
/// Once the first box disconnects, the second one's lane is mirrored.
async fn two_boxes_with_one_project() {
    if !ipv6_loopback().await {
        eprintln!("skipping two_boxes_with_one_project: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT * 2, async {
        let tmp = temp();
        let alpha_repo = repo(&tmp.path, "alpharepo", BOX);
        let beta_repo = repo(
            &tmp.path,
            "betarepo",
            &BOX.replace("hello-from-box", "hello-from-beta"),
        );
        let alpha = BoxSide::start(&tmp, "alpha", "26000-26499").await;
        let beta = BoxSide::start(&tmp, "beta", "26400-26899").await;
        let units = json!({"units": ["web", "echo"]});
        alpha
            .h
            .call(Method::Start, units.clone(), cx(&alpha_repo))
            .await
            .expect("alpha start");
        beta.h
            .call(Method::Start, units, cx(&beta_repo))
            .await
            .expect("beta start");
        let alpha_echo = box_port(&alpha.h, 0, "echo").await;
        let fixed_repo = repo(&tmp.path, "fixedrepo", &fixed_port_project(alpha_echo));
        beta.h
            .call(Method::Start, json!({}), cx(&fixed_repo))
            .await
            .expect("beta fixed start");

        let client = ClientSide::start_with(
            &tmp,
            "client",
            "25000-25499",
            &[("alpha", &alpha), ("beta", &beta)],
            None,
        )
        .await;
        let (session, mut frames, events) = subscribe(&client, 9_101);
        client.connect("alpha").await;
        let routed = eventually(SOON, || async {
            http_get(client.proxy_port, "web.boxapp.localhost")
                .await
                .filter(|response| response.contains("hello-from-box"))
        })
        .await;
        assert!(routed.is_some(), "alpha's web is routed");

        client.connect("beta").await;
        let skipped = eventually(SOON, || async {
            let host = client.host_named("beta").await;
            host["skipped_lanes"]
                .as_array()?
                .iter()
                .find(|lane| lane["lane"] == "boxapp@0")
                .cloned()
        })
        .await
        .expect("beta's boxapp lane is skipped");
        assert_eq!(skipped["box"], "alpha", "{skipped}");
        assert!(
            skipped["reason"]
                .as_str()
                .is_some_and(|r| r.contains("box alpha")),
            "{skipped}"
        );
        let held =
            format!("port {alpha_echo} is in use on this machine by box alpha (boxapp@0:echo)");
        let found = eventually(SOON, || async {
            let host = client.host_named("beta").await;
            warns(&host, &held).then_some(host)
        })
        .await;
        let Some(beta_host) = found else {
            panic!(
                "the port conflict names alpha: {}",
                client.host_named("beta").await
            );
        };
        assert!(
            warns(&beta_host, "overlaps box alpha's (26000-26499)"),
            "{beta_host}"
        );
        assert!(
            !warns(&client.host_named("alpha").await, "overlaps"),
            "alpha connected first, alone"
        );
        let response = http_get(client.proxy_port, "web.boxapp.localhost").await;
        assert!(
            response.is_some_and(|r| r.contains("hello-from-box")),
            "the first box keeps the hostname"
        );

        let seen = warnings(&mut frames).await;
        assert!(
            seen.iter().any(|w| w["host"] == "beta"
                && w["code"] == "lane_skipped"
                && w["box"] == "alpha"
                && w["lane"] == "boxapp@0"),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|w| w["host"] == "beta"
                && w["code"] == "port_in_use"
                && w["box"] == "alpha"
                && w["held_by"] == "boxapp@0:echo"),
            "{seen:?}"
        );
        assert!(
            seen.iter()
                .any(|w| w["host"] == "beta" && w["code"] == "ports_range_overlap"),
            "{seen:?}"
        );

        // Without alpha, beta's lane and port are mirrored.
        client
            .h
            .call(
                Method::Disconnect,
                json!({"host": "alpha"}),
                Context::default(),
            )
            .await
            .expect("disconnect alpha");
        let regained = eventually(SOON, || async {
            http_get(client.proxy_port, "web.boxapp.localhost")
                .await
                .filter(|response| response.contains("hello-from-beta"))
        })
        .await;
        assert!(
            regained.is_some(),
            "beta's lane is mirrored once alpha left"
        );
        let bound = eventually(SOON, || async {
            let host = client.host_named("beta").await;
            let ports = host["ports"].as_array()?.clone();
            ports
                .iter()
                .any(|port| port["port"] == alpha_echo && port["addresses"] != json!([]))
                .then_some(())
        })
        .await;
        assert!(bound.is_some(), "{}", client.host_named("beta").await);

        session.close();
        let _ = events.await;
        client.stop().await;
        alpha.stop().await;
        beta.stop().await;
    })
    .await
    .expect("timeout");
}

/// A local project with one routed HTTP service.
const LOCAL_WEB: &str = r#"project: localweb
services:
  web:
    run: "http hello-from-local"
    hosts: ["web.${{ lane.domain }}"]
"#;

/// The HTTPS port the client reports as its own; nothing listens there.
const CLIENT_HTTPS: u16 = 1;

/// A free loopback port, released for the code under test to bind.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

/// A TLS client that trusts `ca`.
fn tls_client(ca: &CertificateAuthority) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.certificate_der()).expect("root");
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

/// One HTTPS GET to `127.0.0.1:<port>` for `host`, as a browser opening
/// `https://<host>:<port>/` sends it; `None` when the handshake or the request fails.
async fn tls_get(tls: &tokio_rustls::TlsConnector, port: u16, host: &str) -> Option<String> {
    let attempt = async {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        let name = rustls::pki_types::ServerName::try_from(host.to_owned()).ok()?;
        let mut stream = tls.connect(name, tcp).await.ok()?;
        let request = format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut response = Vec::new();
        // A peer that closes without close_notify still delivered the response.
        let _ = stream.read_to_end(&mut response).await;
        Some(String::from_utf8_lossy(&response).into_owned())
    };
    tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .ok()
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
/// A box whose proxy serves HTTPS on another port than the client's: the
/// client serves the box's hostnames on that port too, with its CA, but not
/// its own lanes' hostnames, and closes the port when the box disconnects. A
/// port another process holds is reported with `proxy_port_in_use` and served
/// once `connect` runs again after it is free.
async fn box_https_port_is_served_here() {
    tokio::time::timeout(LIMIT * 2, async {
        let tmp = temp();
        let path = tmp.path.join("ca");
        let ca = Arc::new(
            tokio::task::spawn_blocking(move || CertificateAuthority::load_or_create(&path))
                .await
                .expect("ca task")
                .expect("ca"),
        );
        let box_https = free_port();
        let squatter = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("squatter");
        let busy_https = squatter.local_addr().expect("squatter address").port();

        let box_repo = repo(&tmp.path, "boxrepo", BOX);
        let side = BoxSide::start_on_https(&tmp, "box", "22000-22499", box_https).await;
        side.h
            .call(Method::Start, json!({"units": ["web"]}), cx(&box_repo))
            .await
            .expect("box start");
        let busy = BoxSide::start_on_https(&tmp, "busybox", "22500-22999", busy_https).await;
        let client = ClientSide::start_with(
            &tmp,
            "client",
            "21000-21499",
            &[("box", &side), ("busybox", &busy)],
            Some((CLIENT_HTTPS, ca.clone())),
        )
        .await;
        let local_repo = repo(&tmp.path, "localweb", LOCAL_WEB);
        client
            .h
            .call(Method::Start, json!({}), cx(&local_repo))
            .await
            .expect("local start");
        client.connect("box").await;

        let tls = tls_client(&ca);
        let reached = eventually(SOON, || async {
            tls_get(&tls, box_https, "web.boxapp.localhost")
                .await
                .filter(|response| response.contains("hello-from-box"))
        })
        .await;
        assert!(reached.is_some(), "the box's URL opens on its own port");
        assert_eq!(client.host().await["https_port"], box_https);
        let local = eventually(SOON, || async {
            http_get(client.proxy_port, "web.localweb.localhost")
                .await
                .filter(|response| response.contains("hello-from-local"))
        })
        .await;
        assert!(local.is_some(), "the local lane is routed");
        let wrong = tls_get(&tls, box_https, "web.localweb.localhost").await;
        assert!(
            wrong.is_none_or(|response| !response.contains("hello-from-local")),
            "a local hostname answered on the box's port"
        );

        client.connect("busybox").await;
        let warned = eventually(SOON, || async {
            let host = client.host_named("busybox").await;
            warns(
                &host,
                &format!("HTTPS port {busy_https}, which this machine cannot serve (in use by"),
            )
            .then_some(())
        })
        .await;
        assert!(warned.is_some(), "{}", client.host_named("busybox").await);
        drop(squatter);
        // Connecting again retries the port once it is free.
        client.connect("busybox").await;
        let host = client.host_named("busybox").await;
        assert!(!warns(&host, "cannot serve"), "{host}");
        assert!(
            TcpStream::connect(("127.0.0.1", busy_https)).await.is_ok(),
            "the freed port is served"
        );

        client
            .h
            .call(
                Method::Disconnect,
                json!({"host": "box"}),
                Context::default(),
            )
            .await
            .expect("disconnect");
        let closed = eventually(SOON, || async {
            TcpStream::connect(("127.0.0.1", box_https))
                .await
                .is_err()
                .then_some(())
        })
        .await;
        assert!(
            closed.is_some(),
            "the box's port closes with its connection"
        );

        client.stop().await;
        side.stop().await;
        busy.stop().await;
    })
    .await
    .expect("timeout");
}
