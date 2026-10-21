//! End-to-end protocol tests over the in-memory transport.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_transport::memory::{MemoryTransport, memory_transport};
use devlanes_transport::{
    Backoff, BoxServices, DialTarget, LaneInfo, LaneKey, Link, LinkState, LoopbackDialer,
    OpenError, PortInfo, Registry, RegistryEvent, RegistrySnapshot, RejectReason, RpcCallError,
    RpcError, RpcHandler, RpcInput, RpcReplyStream, ServeOptions, Session, SessionEnd,
    SessionOptions, SharedRegistry, SyncError, SyncFeed, SyncMessage, SyncSource, Timing, UnitInfo,
    UnitKey, UnitKind, serve,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
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

/// Handler covering unary, streaming, input, failure and cancellation.
struct TestRpc {
    dropped: mpsc::UnboundedSender<()>,
}

/// Reports when a reply stream is dropped, proving cancellation reached the box.
struct DropSignal(mpsc::UnboundedSender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[async_trait]
impl RpcHandler for TestRpc {
    async fn call(&self, request: Value, mut input: RpcInput) -> Result<RpcReplyStream, RpcError> {
        match request["op"].as_str() {
            Some("echo") => {
                let value = request["value"].clone();
                Ok(futures_util::stream::once(async move { Ok(value) }).boxed())
            }
            Some("count") => {
                let n = request["n"].as_u64().unwrap_or(0);
                Ok(futures_util::stream::iter((0..n).map(|i| Ok(json!(i)))).boxed())
            }
            Some("collect") => {
                let mut values = Vec::new();
                while let Some(value) = input.recv().await {
                    values.push(value);
                }
                Ok(futures_util::stream::once(async move { Ok(Value::Array(values)) }).boxed())
            }
            Some("hang") => {
                let signal = DropSignal(self.dropped.clone());
                Ok(
                    futures_util::stream::once(async move { Ok(json!("started")) })
                        .chain(futures_util::stream::pending())
                        .map(move |item| {
                            let _keep = &signal;
                            item
                        })
                        .boxed(),
                )
            }
            _ => Err(RpcError::new("unknown op")),
        }
    }
}

/// Sync source that sends a manifest, one chunk and a deletion.
struct TestSync;

#[async_trait]
impl SyncSource for TestSync {
    async fn subscribe(&self, request: Value) -> Result<SyncFeed, SyncError> {
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

/// A box accepting every memory link with the test services.
struct TestBox {
    transport: MemoryTransport,
    registry: SharedRegistry,
    cancelled: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl Drop for TestBox {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start_box(initial: RegistrySnapshot) -> TestBox {
    let (transport, mut listener) = memory_transport();
    let registry = SharedRegistry::new(initial, 64);
    let (dropped, cancelled) = mpsc::unbounded_channel();
    let services = BoxServices {
        registry: Arc::new(registry.clone()),
        rpc: Arc::new(TestRpc { dropped }),
        dialer: Arc::new(LoopbackDialer),
        sync: Arc::new(TestSync),
    };
    let task = tokio::spawn(async move {
        while let Some(link) = listener.accept().await {
            let services = services.clone();
            let mut options = ServeOptions::new("devlanes-test-box");
            options.timing = fast_timing();
            tokio::spawn(async move {
                let _ = serve(link, services, options).await;
            });
        }
    });
    TestBox {
        transport,
        registry,
        cancelled,
        task,
    }
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

#[tokio::test]
async fn rpc_forwarding_supports_unary_streaming_input_and_errors() -> TestResult {
    bounded(async {
        let test_box = start_box(snapshot(Vec::new()));
        let session = Session::connect(&test_box.transport, session_options()).await?;

        let value = session
            .request(json!({"op": "echo", "value": {"x": 1}}))
            .await?;
        assert_eq!(value, json!({"x": 1}));

        let mut call = session.call(json!({"op": "count", "n": 3})).await?;
        call.finish_input().await?;
        let mut items = Vec::new();
        while let Some(item) = call.next().await {
            items.push(item?);
        }
        assert_eq!(items, [json!(0), json!(1), json!(2)]);

        let mut call = session.call(json!({"op": "collect"})).await?;
        call.send(json!("a")).await?;
        call.send(json!("b")).await?;
        call.finish_input().await?;
        assert_eq!(call.next().await.transpose()?, Some(json!(["a", "b"])));
        assert!(call.next().await.is_none());

        let error = session.request(json!({"op": "nope"})).await;
        assert!(matches!(error, Err(RpcCallError::Remote(e)) if e.message == "unknown op"));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn dropping_a_call_cancels_it_on_the_box() -> TestResult {
    bounded(async {
        let mut test_box = start_box(snapshot(Vec::new()));
        let session = Session::connect(&test_box.transport, session_options()).await?;
        let mut call = session.call(json!({"op": "hang"})).await?;
        assert_eq!(call.next().await.transpose()?, Some(json!("started")));
        drop(call);
        test_box
            .cancelled
            .recv()
            .await
            .ok_or("handler stream never dropped")?;
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
        assert!(session.call(json!({"op": "echo"})).await.is_err());
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
        let (dropped, _cancelled) = mpsc::unbounded_channel();
        let services = BoxServices {
            registry: Arc::new(registry),
            rpc: Arc::new(TestRpc { dropped }),
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
