//! A client engine connected to a real box over system OpenSSH, normally an sshd
//! container. The box runs one lane with an HTTP service named
//! `web.<lane domain>`, a TCP echo service and a synced folder; see the
//! environment below. Mirroring, routing and sync then run exactly as in the
//! daemon, with this machine's file system as the sync destination.
//!
//! Environment (as for devlanes-transport-ssh's `needs ssh` tests):
//! - `DEVLANES_TEST_SSH_DEST`, `DEVLANES_TEST_SSH_PORT`, `DEVLANES_TEST_SSH_KEY`,
//!   `DEVLANES_TEST_SSH_KNOWN_HOSTS`: the box and how to reach it.
//! - `DEVLANES_TEST_SSH_BOX_LANE=1`: the box runs project `boxweb` with
//!   services `web` (an HTTP server with `hosts: web.${{ lane.domain }}` and a
//!   `sync` of `ext`) and `echo` (TCP echo). `ext` holds `hello.txt`, the
//!   executable `run.sh`, and names this machine may not hold: `con.txt`,
//!   `a:b`, `trail.` and both `README.md` and `Readme.md`.

use std::{
    net::{IpAddr, Ipv4Addr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use devlanes_daemon::integrations::{ConnectOptions, RemoteConnector, RemotePrompter};
use devlanes_proto::{Context, Method, RpcError};
use devlanes_transport::Transport;
use devlanes_transport_ssh::{SshOptions, SshTransport};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::{ProxyRoutes, engine_with, http_get};
use crate::{LIMIT, eventually, temp};

/// Waits for state changes that cross the network.
const SLOW: Duration = Duration::from_secs(30);

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The file that discards known hosts.
#[cfg(unix)]
const NO_FILE: &str = "/dev/null";
#[cfg(windows)]
const NO_FILE: &str = "NUL";

/// SSH transports to the box from the test environment, mirrored on IPv4
/// loopback only (the box is another machine, so nothing here holds its ports).
struct EnvConnector;

impl RemoteConnector for EnvConnector {
    /// Batch mode: the key in the environment needs no prompt.
    fn transport(
        &self,
        host: &str,
        _prompts: Arc<dyn RemotePrompter>,
        _options: ConnectOptions,
    ) -> Result<Arc<dyn Transport>, RpcError> {
        let home = std::env::temp_dir();
        let mut options = SshOptions::new(home);
        if let Some(port) = env("DEVLANES_TEST_SSH_PORT") {
            options.ssh_options.push(format!("Port={port}"));
        }
        if let Some(key) = env("DEVLANES_TEST_SSH_KEY") {
            options.ssh_options.extend([
                format!("IdentityFile=\"{key}\""),
                "IdentitiesOnly=yes".to_owned(),
                "IdentityAgent=none".to_owned(),
            ]);
        }
        if let Some(known_hosts) = env("DEVLANES_TEST_SSH_KNOWN_HOSTS") {
            options.ssh_options.extend([
                format!("UserKnownHostsFile=\"{known_hosts}\""),
                format!("GlobalKnownHostsFile={NO_FILE}"),
                "StrictHostKeyChecking=yes".to_owned(),
            ]);
        }
        Ok(Arc::new(SshTransport::new(host, options)))
    }

    fn kind(&self) -> &'static str {
        "ssh"
    }

    fn mirror_addresses(&self) -> Vec<IpAddr> {
        vec![Ipv4Addr::LOCALHOST.into()]
    }
}

/// The `host ls` entry of `host`.
async fn host_entry(h: &crate::Harness, host: &str) -> Value {
    let list = h
        .call(Method::HostList, json!({}), Context::default())
        .await
        .expect("host ls");
    list["items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["host"] == host))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Round-trips a line through `127.0.0.1:<port>`.
async fn echo(port: u16) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.ok()?;
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

/// The synced folder's status once its initial pass is done.
async fn watching(h: &crate::Harness, host: &str) -> Option<Value> {
    let host = host_entry(h, host).await;
    let sync = host["syncs"].as_array()?.first()?.clone();
    (sync["state"] == "watching").then_some(sync)
}

/// Whether a file of the synced folder is `contents`.
fn holds(dir: &Path, name: &str, contents: &[u8]) -> bool {
    std::fs::read(dir.join(name)).is_ok_and(|read| read == contents)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ssh"]
/// Connect over real ssh mirrors the box's ports and hostname, syncs its folder
/// (refusing names this file system cannot hold or tell apart), sends nothing
/// again on a second connection, and disconnect unbinds.
async fn ssh_box_mirrors_routes_and_syncs() {
    if env("DEVLANES_TEST_SSH_BOX_LANE").as_deref() != Some("1") {
        eprintln!("skipping ssh_box_mirrors_routes_and_syncs: no box lane set up");
        return;
    }
    let host = env("DEVLANES_TEST_SSH_DEST").unwrap_or_else(|| "localhost".to_owned());
    tokio::time::timeout(LIMIT * 3, async {
        let tmp = temp();
        let proxy = devlanes_proxy::Proxy::new(
            devlanes_proxy::ProxyConfig::default(),
            devlanes_proxy::RouteTable::default(),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("proxy listener");
        let proxy_port = listener.local_addr().expect("proxy addr").port();
        let proxy_stop = tokio_util::sync::CancellationToken::new();
        let serving = proxy.clone();
        let stop = proxy_stop.clone();
        tokio::spawn(async move {
            let _ = serving.serve(listener, stop).await;
        });
        let client = engine_with(
            &tmp.path.join("client"),
            "28500-28999",
            Some(Arc::new(EnvConnector)),
            Some(Arc::new(ProxyRoutes(proxy))),
        )
        .await;

        let connected = client
            .call(
                Method::Connect,
                json!({"host": host, "background": true}),
                Context::default(),
            )
            .await
            .expect("connect");
        assert_eq!(connected["state"], "connected", "{connected}");

        // Both box services are mirrored under their own numbers.
        let ports = eventually(SLOW, || async {
            let entry = host_entry(&client, &host).await;
            let ports: Vec<u16> = entry["ports"]
                .as_array()?
                .iter()
                .filter_map(|port| u16::try_from(port["port"].as_u64()?).ok())
                .collect();
            (ports.len() >= 2).then_some(ports)
        })
        .await
        .expect("box ports mirrored");
        let echoed = eventually(SLOW, || async {
            for port in &ports {
                if echo(*port).await {
                    return Some(*port);
                }
            }
            None
        })
        .await;
        assert!(echoed.is_some(), "raw TCP through ssh: {ports:?}");

        // HTTP through the local proxy reaches the box's web service.
        let response = eventually(SLOW, || async {
            http_get(proxy_port, "web.boxweb.localhost")
                .await
                .filter(|response| response.contains("\"path\":\"/\""))
        })
        .await;
        assert!(response.is_some(), "proxied request reached the box");

        // Sync: plain files arrive; names this machine cannot hold are
        // refused and reported instead of written somewhere else.
        let out = tmp.path.join("client").join(format!(
            "sync/{}/boxweb/boxweb@0/ext",
            devlanes_core::paths::name_on_disk(&host.replace('/', "_"))
        ));
        let first = eventually(SLOW, || async {
            let sync = watching(&client, &host).await?;
            holds(&out, "hello.txt", b"hello from the box\n").then_some(sync)
        })
        .await;
        let Some(first) = first else {
            panic!("sync finished: {}", host_entry(&client, &host).await);
        };
        assert!(holds(&out, "run.sh", b"#!/bin/sh\necho run\n"), "{first}");
        let errors = first["errors"].to_string();
        let held: Vec<String> = std::fs::read_dir(&out)
            .expect("destination")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        #[cfg(windows)]
        {
            for refused in ["con.txt", "a:b", "trail."] {
                assert!(errors.contains(refused), "{refused} not reported: {first}");
            }
            assert!(!out.join("a").exists(), "`a:b` became a stream of `a`");
            let readmes: Vec<&String> = held
                .iter()
                .filter(|name| name.eq_ignore_ascii_case("readme.md"))
                .collect();
            assert_eq!(readmes.len(), 1, "{held:?}");
            assert!(errors.contains("differs only by case"), "{first}");
        }
        #[cfg(unix)]
        assert!(held.iter().any(|name| name == "a:b"), "{held:?} {errors}");

        // A second connection compares and writes nothing that is already here.
        client
            .call(
                Method::Disconnect,
                json!({"host": host}),
                Context::default(),
            )
            .await
            .expect("disconnect");
        let unbound = eventually(SLOW, || async {
            let mut bound = false;
            for port in &ports {
                bound |= TcpStream::connect((Ipv4Addr::LOCALHOST, *port))
                    .await
                    .is_ok();
            }
            (!bound).then_some(())
        })
        .await;
        assert!(unbound.is_some(), "listeners close on disconnect");
        let again = client
            .call(
                Method::Connect,
                json!({"host": host, "background": true}),
                Context::default(),
            )
            .await
            .expect("connect again");
        assert_eq!(again["state"], "connected", "{again}");
        let second = eventually(SLOW, || async {
            let sync = watching(&client, &host).await?;
            (sync["compared"].as_u64()? > 0).then_some(sync)
        })
        .await;
        let Some(second) = second else {
            panic!("second sync: {}", host_entry(&client, &host).await);
        };
        // The executable `run.sh` included: a Windows client asks the box to
        // compare modes by write access only.
        assert_eq!(second["written"], 0, "{second}");

        client
            .call(
                Method::Disconnect,
                json!({"host": host}),
                Context::default(),
            )
            .await
            .expect("disconnect");
        proxy_stop.cancel();
        client.close().await;
    })
    .await
    .expect("timeout");
}
