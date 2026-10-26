//! End-to-end protocol tests over the in-memory transport.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_transport::memory::{MemoryTransport, memory_transport};
use devlanes_transport::{
    Backoff, BoxError, BoxInfo, BoxServices, ClientInfo, DialTarget, LaneInfo, LaneKey, Link,
    LinkState, LoopbackDialer, OpenError, PortInfo, Registry, RegistryEvent, RegistryFeed,
    RegistrySnapshot, RegistrySource, RegistryUpdate, RejectReason, ServeError, ServeOptions,
    Session, SessionEnd, SessionOptions, SharedRegistry, SyncError, SyncFeed, SyncInput,
    SyncInputEvent, SyncMessage, SyncSource, Timing, UnitInfo, UnitKey, UnitKind, serve,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Fails the test instead of hanging when a future never completes.
async fn bounded<F: Future<Output = TestResult>>(future: F) -> TestResult {
    tokio::time::timeout(Duration::from_secs(10), future).await?
}

fn fast_timing() -> Timing {
    Timing {
        ping_interval: Duration::from_millis(100),
        ping_timeout: Duration::from_millis(300),
        handshake_timeout: Duration::from_secs(2),
    }
}

fn lane() -> LaneKey {
    LaneKey {
        project: "app".into(),
        lane: "main".into(),
    }
}

fn unit(name: &str, port: u16, hidden: bool) -> UnitInfo {
    UnitInfo {
        key: UnitKey {
            lane: lane(),
            kind: UnitKind::Service,
            name: name.into(),
        },
        hidden,
        state: "running".into(),
        ports: vec![PortInfo {
            name: "default".into(),
            port,
        }],
        hosts: Vec::new(),
    }
}

fn snapshot(units: Vec<UnitInfo>) -> RegistrySnapshot {
    RegistrySnapshot {
        lanes: vec![LaneInfo {
            key: lane(),
            index: 0,
            domain: Some("app.localhost".into()),
        }],
        units,
    }
}

/// Sync source that sends a manifest, one chunk and a deletion.
struct TestSync;

#[async_trait]
impl SyncSource for TestSync {
    async fn subscribe(&self, request: Value, mut input: SyncInput) -> Result<SyncFeed, SyncError> {
        if request["folder"] == "echo" {
            // Collects the client's input up to its end marker, then reports it.
            let feed = async_stream_once(async move {
                let mut values = Vec::new();
                loop {
                    match input.recv().await {
                        Some(SyncInputEvent::Value(value)) => values.push(value),
                        Some(SyncInputEvent::End) => break,
                        None => return Err(SyncError::new("client left without ending input")),
                    }
                }
                Ok(SyncMessage::Manifest(Value::Array(values)))
            });
            return Ok(feed);
        }
        if request["folder"] != "ext" {
            return Err(SyncError::new("unknown folder"));
        }
        let messages = vec![
            Ok(SyncMessage::Manifest(json!({"files": ["a.js"]}))),
            Ok(SyncMessage::Chunk {
                path: "a.js".into(),
                offset: 0,
                data: Bytes::from_static(b"\0binary\xff"),
                last: true,
            }),
            Ok(SyncMessage::Delete {
                path: "old.js".into(),
            }),
        ];
        Ok(futures_util::stream::iter(messages).boxed())
    }
}

/// A feed of the one item `future` yields.
fn async_stream_once(
    future: impl Future<Output = Result<SyncMessage, SyncError>> + Send + 'static,
) -> SyncFeed {
    futures_util::stream::once(future).boxed()
}

/// A box accepting every memory link with the test services.
struct TestBox {
    transport: MemoryTransport,
    registry: SharedRegistry,
    task: JoinHandle<()>,
}

impl Drop for TestBox {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start_box(initial: RegistrySnapshot) -> TestBox {
    let mut options = ServeOptions::new("devlanes-test-box");
    options.timing = fast_timing();
    start_box_with(initial, options)
}

/// [`start_box`] with its own serve options.
fn start_box_with(initial: RegistrySnapshot, options: ServeOptions) -> TestBox {
    let (transport, mut listener) = memory_transport();
    let registry = SharedRegistry::new(initial, 64);
    let services = BoxServices {
        registry: Arc::new(registry.clone()),
        dialer: Arc::new(LoopbackDialer),
        sync: Arc::new(TestSync),
    };
    let task = tokio::spawn(async move {
        while let Some(link) = listener.accept().await {
            let services = services.clone();
            let options = options.clone();
            tokio::spawn(async move {
                let _ = serve(link, services, options).await;
            });
        }
    });
    TestBox {
        transport,
        registry,
        task,
    }
}

/// Names as owned strings.
fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn session_options() -> SessionOptions {
    let mut options = SessionOptions::new("devlanes-test-client");
    options.timing = fast_timing();
    options
}

/// Echo server on an ephemeral loopback port, standing in for a service.
async fn echo_server() -> Result<(u16, JoinHandle<()>), std::io::Error> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = socket.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    Ok((port, task))
}

async fn echo_roundtrip(session: &Session, port: u16) -> TestResult {
    let mut stream = session.open(DialTarget::Port { port }).await?;
    stream.write_all(b"hello through the mux").await?;
    let mut buf = [0u8; 21];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello through the mux");
    Ok(())
}

#[tokio::test]
async fn control_stream_delivers_snapshot() -> TestResult {
    bounded(async {
        let initial = snapshot(vec![unit("web", 20000, false), unit("db", 20001, true)]);
        let test_box = start_box(initial.clone());
        let session = Session::connect(&test_box.transport, session_options()).await?;
        assert_eq!(session.server(), "devlanes-test-box");
        let registry = session.registry().current();
        assert_eq!(registry, Registry::from_snapshot(initial));
        assert_eq!(
            registry
                .mirrored_ports()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [20000]
        );
        Ok(())
    })
    .await
}

/// A feature both ends list is used; one only one end lists is not, and a
/// client without features gets none. The box's info reaches the client.
#[tokio::test]
async fn features_are_negotiated_and_box_info_reaches_the_client() -> TestResult {
    bounded(async {
        let mut options = ServeOptions::new("devlanes-test-box");
        options.timing = fast_timing();
        options.features = names(&["shared", "box_only"]);
        options.box_info = BoxInfo {
            https_port: Some(8443),
            ports_range: Some("20000-29999".into()),
        };
        let test_box = start_box_with(snapshot(Vec::new()), options);

        let mut with = session_options();
        with.features = names(&["client_only", "shared"]);
        let session = Session::connect(&test_box.transport, with).await?;
        assert_eq!(session.features(), ["shared"]);
        assert!(session.has_feature("shared"));
        assert!(!session.has_feature("box_only"));
        assert!(!session.has_feature("client_only"));
        assert_eq!(session.box_info().https_port, Some(8443));
        assert_eq!(
            session.box_info().ports_range.as_deref(),
            Some("20000-29999")
        );

        let mut without = session_options();
        without.features = Vec::new();
        let session = Session::connect(&test_box.transport, without).await?;
        assert!(session.features().is_empty());
        assert!(!session.has_feature("shared"));
        Ok(())
    })
    .await
}

/// A registry source that records the client it serves and hands out a feed
/// whose updates the test controls.
struct ScriptedRegistry {
    seen: std::sync::Mutex<Option<oneshot::Sender<ClientInfo>>>,
    updates: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Result<RegistryUpdate, BoxError>>>>,
}

#[async_trait]
impl RegistrySource for ScriptedRegistry {
    /// An empty snapshot, then whatever the test sends.
    async fn subscribe(&self, client: &ClientInfo) -> Result<RegistryFeed, BoxError> {
        if let Some(seen) = self.seen.lock().map_err(|e| e.to_string())?.take() {
            let _ = seen.send(client.clone());
        }
        let receiver = self
            .updates
            .lock()
            .map_err(|e| e.to_string())?
            .take()
            .ok_or("subscribed twice")?;
        let updates = futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|update| (update, receiver))
        })
        .boxed();
        Ok(RegistryFeed {
            snapshot: RegistrySnapshot::default(),
            updates,
        })
    }
}

/// The box's registry source learns the client's identity and the negotiated
/// features; a feed that fails ends the session on both ends, with the
/// source's error on the box.
#[tokio::test]
async fn a_failing_registry_feed_ends_the_session() -> TestResult {
    bounded(async {
        let (transport, mut listener) = memory_transport();
        let (seen_tx, seen) = oneshot::channel();
        let (updates, receiver) = mpsc::unbounded_channel();
        let services = BoxServices {
            registry: Arc::new(ScriptedRegistry {
                seen: std::sync::Mutex::new(Some(seen_tx)),
                updates: std::sync::Mutex::new(Some(receiver)),
            }),
            dialer: Arc::new(LoopbackDialer),
            sync: Arc::new(TestSync),
        };
        let server = tokio::spawn(async move {
            let link = listener.accept().await.ok_or("no link")?;
            let mut options = ServeOptions::new("box");
            options.timing = fast_timing();
            options.features = names(&["shared"]);
            serve(link, services, options)
                .await
                .map_err(|error| error.to_string())
        });
        let mut options = session_options();
        options.client_id = "0123abcd".into();
        options.client_name = "laptop".into();
        options.features = names(&["shared", "other"]);
        let session = Session::connect(&transport, options).await?;
        let client = seen.await?;
        assert_eq!(client.id, "0123abcd");
        assert_eq!(client.name, "laptop");
        assert_eq!(client.label, "devlanes-test-client");
        assert_eq!(client.features, ["shared"]);

        updates.send(Err("the box daemon sent a malformed registry".into()))?;
        let end = tokio::time::timeout(Duration::from_secs(5), session.closed()).await?;
        assert_ne!(end, SessionEnd::Closed, "{end}");
        let result = server.await?;
        let error = result.expect_err("serve failed");
        assert!(error.contains("malformed registry"), "{error}");
        Ok(())
    })
    .await
}

/// A feed that simply ends also ends the session.
#[tokio::test]
async fn a_closed_registry_feed_ends_the_session() -> TestResult {
    bounded(async {
        let (transport, mut listener) = memory_transport();
        let (updates, receiver) = mpsc::unbounded_channel::<Result<RegistryUpdate, BoxError>>();
        let services = BoxServices {
            registry: Arc::new(ScriptedRegistry {
                seen: std::sync::Mutex::new(None),
                updates: std::sync::Mutex::new(Some(receiver)),
            }),
            dialer: Arc::new(LoopbackDialer),
            sync: Arc::new(TestSync),
        };
        let server = tokio::spawn(async move {
            let link = listener.accept().await.ok_or("no link")?;
            let mut options = ServeOptions::new("box");
            options.timing = fast_timing();
            let result = serve(link, services, options).await;
            Ok::<_, String>(matches!(result, Err(ServeError::RegistryClosed)))
        });
        let session = Session::connect(&transport, session_options()).await?;
        drop(updates);
        let end = tokio::time::timeout(Duration::from_secs(5), session.closed()).await?;
        assert_ne!(end, SessionEnd::Closed, "{end}");
        assert!(server.await??, "serve ended with RegistryClosed");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn registered_port_is_dialed() -> TestResult {
    bounded(async {
        let (port, _echo) = echo_server().await?;
        let test_box = start_box(snapshot(vec![unit("api", port, false)]));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        echo_roundtrip(&session, port).await
    })
    .await
}

#[tokio::test]
async fn stopping_a_unit_closes_its_streams_and_refuses_new_ones() -> TestResult {
    bounded(async {
        let (port, _echo) = echo_server().await?;
        let test_box = start_box(snapshot(vec![unit("api", port, false)]));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        let mut stream = session.open(DialTarget::Port { port }).await?;
        stream.write_all(b"before").await?;
        let mut buf = [0u8; 6];
        stream.read_exact(&mut buf).await?;
        let mut stopped = unit("api", port, false);
        stopped.state = "stopped".into();
        test_box
            .registry
            .publish(RegistryEvent::UnitUpserted { unit: stopped });
        // The open stream ends: reads reach EOF (or fail) instead of hanging.
        let mut rest = Vec::new();
        let ended =
            tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut rest)).await;
        assert!(ended.is_ok(), "stream to a stopped unit stays open");
        let refused = session.open(DialTarget::Port { port }).await;
        assert!(
            matches!(
                refused,
                Err(OpenError::Rejected {
                    reason: RejectReason::NotRunning,
                    ..
                })
            ),
            "{refused:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn unregistered_and_hidden_ports_are_rejected() -> TestResult {
    bounded(async {
        let (port, _echo) = echo_server().await?;
        let (hidden_port, _hidden_echo) = echo_server().await?;
        let test_box = start_box(snapshot(vec![unit("db", hidden_port, true)]));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        let result = session.open(DialTarget::Port { port }).await;
        assert!(matches!(
            result,
            Err(OpenError::Rejected {
                reason: RejectReason::NotRegistered,
                ..
            })
        ));
        let result = session.open(DialTarget::Port { port: hidden_port }).await;
        assert!(matches!(
            result,
            Err(OpenError::Rejected {
                reason: RejectReason::Hidden,
                ..
            })
        ));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn registry_changes_apply_mid_session() -> TestResult {
    bounded(async {
        let (port, _echo) = echo_server().await?;
        let test_box = start_box(snapshot(Vec::new()));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        let mut watch = session.registry().subscribe();
        assert!(session.open(DialTarget::Port { port }).await.is_err());

        test_box.registry.publish(RegistryEvent::UnitUpserted {
            unit: unit("late", port, false),
        });
        watch
            .wait_for(|registry| registry.mirrored_ports().contains_key(&port))
            .await?;
        echo_roundtrip(&session, port).await?;

        test_box.registry.publish(RegistryEvent::UnitRemoved {
            unit: unit("late", port, false).key,
        });
        watch
            .wait_for(|registry| registry.mirrored_ports().is_empty())
            .await?;
        let result = session.open(DialTarget::Port { port }).await;
        assert!(matches!(result, Err(OpenError::Rejected { .. })));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn sync_subscription_carries_manifest_chunks_and_deletions() -> TestResult {
    bounded(async {
        let test_box = start_box(snapshot(Vec::new()));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        let mut sync = session.subscribe_sync(json!({"folder": "ext"})).await?;
        let mut messages = Vec::new();
        while let Some(message) = sync.next().await {
            messages.push(message?);
        }
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            &messages[1],
            SyncMessage::Chunk { path, data, last: true, .. }
                if path == "a.js" && data.as_ref() == b"\0binary\xff"
        ));

        let mut refused = session.subscribe_sync(json!({"folder": "other"})).await?;
        assert!(matches!(refused.next().await, Some(Err(_))));

        // Client input reaches the source in order, and the end marker is explicit.
        let mut echo = session.subscribe_sync(json!({"folder": "echo"})).await?;
        for batch in 0..20 {
            echo.send(json!({"batch": batch})).await?;
        }
        echo.finish_input().await?;
        match echo.next().await {
            Some(Ok(SyncMessage::Manifest(Value::Array(values)))) => {
                assert_eq!(values.len(), 20);
                assert_eq!(values[19], json!({"batch": 19}));
            }
            other => panic!("unexpected {other:?}"),
        }
        Ok(())
    })
    .await
}

#[tokio::test]
async fn frozen_link_hits_ping_timeout() -> TestResult {
    bounded(async {
        let test_box = start_box(snapshot(Vec::new()));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        let link = test_box.transport.last_link().ok_or("no link")?;
        link.freeze();
        let started = tokio::time::Instant::now();
        assert_eq!(session.closed().await, SessionEnd::PingTimeout);
        // One interval to send the ping plus the timeout, with scheduling slack.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            session
                .subscribe_sync(json!({"folder": "ext"}))
                .await
                .is_err()
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn reconnect_replaces_client_state_with_new_snapshot() -> TestResult {
    bounded(async {
        let test_box = start_box(snapshot(vec![unit("old", 20000, false)]));
        let backoff = Backoff::new(Duration::from_millis(20), Duration::from_millis(100));
        let link = Link::spawn(
            Arc::new(test_box.transport.clone()),
            session_options(),
            backoff,
        );
        let first = link.connected().await?;
        assert!(
            link.registry()
                .current()
                .mirrored_ports()
                .contains_key(&20000)
        );

        // Refuse once so the link has to back off, then change state while it is down.
        test_box.transport.refuse(false);
        test_box.transport.last_link().ok_or("no link")?.sever();
        assert_ne!(first.closed().await, SessionEnd::Closed);
        let mut state = link.state();
        state
            .wait_for(|state| matches!(state, LinkState::Retrying { .. }))
            .await?;
        // Disconnected clients keep the last known state so mirrors stay bound.
        assert!(
            link.registry()
                .current()
                .mirrored_ports()
                .contains_key(&20000)
        );
        test_box
            .registry
            .replace(snapshot(vec![unit("new", 20001, false)]));
        test_box.transport.accept();

        let mut watch = link.registry().subscribe();
        watch
            .wait_for(|registry| {
                let ports: Vec<_> = registry.mirrored_ports().keys().copied().collect();
                ports == [20001]
            })
            .await?;
        let second = link.connected().await?;
        assert!(second.end().is_none());
        assert!(test_box.transport.connections() >= 2);
        link.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn permanent_transport_failure_stops_the_link() -> TestResult {
    bounded(async {
        let test_box = start_box(snapshot(Vec::new()));
        test_box.transport.refuse(true);
        let link = Link::spawn(
            Arc::new(test_box.transport.clone()),
            session_options(),
            Backoff::default(),
        );
        assert!(link.connected().await.is_err());
        assert!(matches!(&*link.state().borrow(), LinkState::Failed { .. }));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn box_closes_silent_client() -> TestResult {
    bounded(async {
        let (transport, mut listener) = memory_transport();
        let registry = SharedRegistry::new(snapshot(Vec::new()), 8);
        let services = BoxServices {
            registry: Arc::new(registry),
            dialer: Arc::new(LoopbackDialer),
            sync: Arc::new(TestSync),
        };
        let server = tokio::spawn(async move {
            let link = listener.accept().await.ok_or("no link")?;
            let mut options = ServeOptions::new("box");
            options.timing = fast_timing();
            serve(link, services, options)
                .await
                .map_err(|e| e.to_string())
        });
        let session = Session::connect(&transport, session_options()).await?;
        transport.last_link().ok_or("no link")?.freeze();
        let result = server.await?;
        assert!(result.is_err_and(|e| e.contains("stopped responding")));
        drop(session);
        Ok(())
    })
    .await
}
