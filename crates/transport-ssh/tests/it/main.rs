//! Real SSH checks, normally run through `e2e/ssh/run.sh` against a throwaway sshd container.
//!
//! Environment:
//! - `DEVLANES_TEST_SSH_DEST`: ssh destination, default `localhost`.
//! - `DEVLANES_TEST_SSH_PORT`, `DEVLANES_TEST_SSH_KEY`, `DEVLANES_TEST_SSH_KNOWN_HOSTS`:
//!   optional; a key also disables the agent so only that key is offered.
//! - `DEVLANES_TEST_SSH_ALLOW_MISSING=1`: the box has no devlanes; the tunnel test skips.
//! - `DEVLANES_TEST_SSH_LANE=1`: `run.sh` started a lane with an echo service and a
//!   synced folder on the box; the mirroring test needs it.

// The transport is Unix only; the crate is empty on Windows.
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use devlanes_transport::{
    DialTarget, RpcCallError, Session, SessionOptions, SyncMessage, Transport,
};
use devlanes_transport_ssh::{SshOptions, SshTransport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn allow_missing() -> bool {
    env("DEVLANES_TEST_SSH_ALLOW_MISSING").as_deref() == Some("1")
}

/// Builds a transport from the test environment; paths are quoted for ssh's option parser.
fn transport(home: &Path) -> SshTransport {
    let mut options = SshOptions::new(home);
    if let Some(port) = env("DEVLANES_TEST_SSH_PORT") {
        options.ssh_options.push(format!("Port={port}"));
    }
    if let Some(key) = env("DEVLANES_TEST_SSH_KEY") {
        options.ssh_options.extend([
            format!("IdentityFile=\"{key}\""),
            "IdentitiesOnly=yes".to_owned(),
            // An agent such as 1Password could prompt or offer other keys.
            "IdentityAgent=none".to_owned(),
        ]);
    }
    if let Some(known_hosts) = env("DEVLANES_TEST_SSH_KNOWN_HOSTS") {
        options.ssh_options.extend([
            format!("UserKnownHostsFile=\"{known_hosts}\""),
            "GlobalKnownHostsFile=/dev/null".to_owned(),
            "StrictHostKeyChecking=yes".to_owned(),
        ]);
    }
    let destination = env("DEVLANES_TEST_SSH_DEST").unwrap_or_else(|| "localhost".to_owned());
    SshTransport::new(destination, options)
}

/// Short home so the control socket fits in `sun_path`.
fn short_home() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new().prefix("dl").tempdir_in("/tmp")
}

#[tokio::test]
#[ignore = "needs ssh"]
async fn ssh_master_and_probe() -> TestResult {
    tokio::time::timeout(Duration::from_secs(60), async {
        let home = short_home()?;
        let transport = transport(home.path());
        let probe = transport.probe().await?;
        assert!(!probe.platform.os.is_empty(), "{probe:?}");
        assert!(!probe.platform.arch.is_empty(), "{probe:?}");
        if !allow_missing() {
            assert!(
                probe.program.is_some(),
                "devlanes not found on the box: {probe:?}"
            );
        }
        // The cached probe and the live master are reused rather than relaunched.
        assert_eq!(transport.probe().await?, probe);
        let masters = std::fs::read_dir(home.path().join("run/ssh"))?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
            .count();
        assert_eq!(masters, 1);
        transport.close().await;
        Ok(())
    })
    .await?
}

#[tokio::test]
#[ignore = "needs ssh"]
async fn ssh_tunnel_session() -> TestResult {
    tokio::time::timeout(Duration::from_secs(60), async {
        let home = short_home()?;
        let transport = transport(home.path());
        if transport.probe().await?.program.is_none() {
            if allow_missing() {
                eprintln!("skipping ssh_tunnel_session: devlanes is not installed on the box");
                return Ok(());
            }
            return Err("devlanes is not installed on the box".into());
        }
        // A working tunnel waits for the client's hello or starts the mux (yamux opens
        // with an RTT ping); an unimplemented one exits at once, and the stream
        // surfaces its stderr as the read error.
        let mut raw = transport.connect().await?;
        let mut byte = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(3), raw.read(&mut byte)).await {
            Err(_) | Ok(Ok(1..)) => {}
            Ok(Err(error)) => {
                let message = error.to_string();
                let lower = message.to_lowercase();
                if lower.contains("not_supported_yet") || lower.contains("not supported yet") {
                    eprintln!(
                        "skipping ssh_tunnel_session: box reports not_supported_yet: {message}"
                    );
                    return Ok(());
                }
                return Err(error.into());
            }
            Ok(Ok(_)) => return Err("tunnel closed its output without an error".into()),
        }
        drop(raw);
        let session = Session::connect(&transport, SessionOptions::new("ssh-test")).await?;
        assert!(!session.server().is_empty());
        // Remote is mirroring only: the box refuses daemon requests.
        let request = serde_json::json!({"method": "daemon.shutdown", "params": {}});
        match session.request(request).await {
            Err(RpcCallError::Remote(error)) => {
                assert_eq!(error.message, "not_supported", "{error:?}");
            }
            other => return Err(format!("the box answered a daemon request: {other:?}").into()),
        }
        session.close();
        transport.close().await;
        Ok(())
    })
    .await?
}

#[tokio::test]
#[ignore = "needs ssh"]
async fn ssh_mirrors_a_port_and_syncs_a_file() -> TestResult {
    if env("DEVLANES_TEST_SSH_LANE").as_deref() != Some("1") {
        eprintln!("skipping ssh_mirrors_a_port_and_syncs_a_file: no lane set up on the box");
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        let home = short_home()?;
        let transport = transport(home.path());
        let session = Session::connect(&transport, SessionOptions::new("ssh-test")).await?;
        // The box's running echo service is in the registry with its port.
        let mut registry = session.registry().subscribe();
        let port = loop {
            let found = registry
                .borrow_and_update()
                .units()
                .find(|unit| unit.key.name == "echo" && unit.state == "ready" && !unit.hidden)
                .and_then(|unit| unit.ports.first().map(|port| port.port));
            if let Some(port) = found {
                break port;
            }
            registry.changed().await?;
        };
        let mut stream = session.open(DialTarget::Port { port }).await?;
        stream.write_all(b"over ssh\n").await?;
        let mut echoed = [0u8; 9];
        stream.read_exact(&mut echoed).await?;
        assert_eq!(&echoed, b"over ssh\n");
        drop(stream);

        // One sync entry, followed from an empty destination.
        let mut list = session
            .subscribe_sync(serde_json::json!({"op": "list"}))
            .await?;
        let entries = match list.next().await {
            Some(Ok(SyncMessage::Manifest(manifest))) => manifest["entries"].clone(),
            other => return Err(format!("no sync list: {other:?}").into()),
        };
        let entry = entries
            .as_array()
            .and_then(|entries| entries.first())
            .ok_or("the box lists no sync entry")?;
        let follow = serde_json::json!({
            "op": "follow",
            "lane": entry["lane"],
            "unit": entry["unit"],
            "index": entry["index"],
        });
        let mut feed = session.subscribe_sync(follow).await?;
        // An empty destination: no manifest batches, just the end marker.
        feed.finish_input().await?;
        let mut contents = Vec::new();
        while let Some(message) = feed.next().await {
            match message? {
                SyncMessage::Chunk {
                    path, data, last, ..
                } if path == "hello.txt" => {
                    contents.extend_from_slice(&data);
                    if last {
                        break;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(contents, b"hello from the box\n");
        session.close();
        transport.close().await;
        Ok(())
    })
    .await?
}
