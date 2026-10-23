#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use bytes::Bytes;
use devlanes_client::{Client, Context, Error, Frame, Hello, Method, Paths};
use devlanes_ipc::{Connection, local::LocalListener};
use devlanes_proto::{Control, Event, Outcome, Response, RpcError};
use serde_json::json;
use tokio::time::timeout;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Keeps mock sockets inside test cleanup even when the host exports
/// XDG_RUNTIME_DIR. A Windows pipe is named after the home, so it is unique.
fn mock_paths(home: &std::path::Path) -> devlanes_client::Result<Paths> {
    #[cfg_attr(windows, expect(unused_mut, reason = "the pipe name stays"))]
    let mut paths = Paths::new(home)?;
    #[cfg(unix)]
    {
        paths.socket = paths.run.join("daemon.sock");
    }
    Ok(paths)
}

/// A fake daemon's endpoint at `paths.socket`.
async fn listen(paths: &Paths) -> Result<LocalListener, devlanes_ipc::local::BindError> {
    LocalListener::bind(&paths.socket).await
}

/// A real socket exercises peer credentials, framing, request context, and queued output together.
#[tokio::test]
async fn request_preserves_events_bytes_and_rpc_errors() -> TestResult {
    timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let paths = mock_paths(&temp.path().join("home"))?;
        paths.prepare()?;
        let mut listener = listen(&paths).await?;
        let server = tokio::spawn(async move {
            let stream = listener.accept().await?;
            let mut connection = Connection::new(stream);
            assert!(matches!(
                connection.recv().await?,
                Frame::Control(Control::Hello(_))
            ));
            connection
                .send(Frame::Control(Control::Hello(Hello::current(3))))
                .await?;
            let Frame::Control(Control::Request(request)) = connection.recv().await? else {
                panic!("missing request")
            };
            assert_eq!(request.method, Method::Status);
            assert_eq!(request.params, json!({"limit": 4}));
            connection
                .send(Frame::Control(Control::Event(Event {
                    seq: 1,
                    event: "changed".into(),
                    data: json!({}),
                })))
                .await?;
            connection
                .send(Frame::Bytes {
                    stream_id: 7,
                    data: Bytes::from_static(&[0, 255]),
                })
                .await?;
            connection
                .send(Frame::Control(Control::Response(Response {
                    id: request.id,
                    result: Outcome::Ok {
                        data: json!({"ok":true}),
                    },
                })))
                .await?;
            let Frame::Control(Control::Request(request)) = connection.recv().await? else {
                panic!("missing request")
            };
            connection
                .send(Frame::Control(Control::Response(Response {
                    id: request.id,
                    result: Outcome::Error {
                        error: RpcError::new("not_found", json!({"unit":"api"}), 3),
                    },
                })))
                .await?;
            Ok::<_, devlanes_ipc::Error>(())
        });
        let mut client = Client::connect(&paths).await?;
        assert_eq!(client.peer.managed_processes, 3);
        assert_eq!(
            client
                .request(Method::Status, json!({"limit":4}), Context::default())
                .await?,
            json!({"ok":true})
        );
        assert!(matches!(
            client.next_frame().await?,
            Frame::Control(Control::Event(Event { seq: 1, .. }))
        ));
        match client.next_frame().await? {
            Frame::Bytes { stream_id, data } => {
                assert_eq!(stream_id, 7);
                assert_eq!(data.as_ref(), &[0, 255]);
            }
            _ => panic!("missing raw frame"),
        }
        assert!(matches!(
            client
                .request(Method::Status, json!({}), Context::default())
                .await,
            Err(Error::Rpc(RpcError { exit_code: 3, .. }))
        ));
        server.await??;
        Ok(())
    })
    .await?
}

/// Mismatch closes only this client and exposes active process information without shutdown.
#[tokio::test]
async fn incompatible_daemon_is_never_sent_shutdown() -> TestResult {
    timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let paths = mock_paths(&temp.path().join("home"))?;
        paths.prepare()?;
        let mut listener = listen(&paths).await?;
        let server = tokio::spawn(async move {
            let stream = listener.accept().await?;
            let mut connection = Connection::new(stream);
            connection.recv().await?;
            let mut hello = Hello::current(42);
            hello.protocol += 1;
            connection
                .send(Frame::Control(Control::Hello(hello)))
                .await?;
            assert!(matches!(
                connection.recv().await,
                Err(devlanes_ipc::Error::Disconnected)
            ));
            Ok::<_, devlanes_ipc::Error>(())
        });
        assert!(matches!(
            Client::connect_or_spawn(&paths).await,
            Err(Error::Incompatible {
                peer: Hello {
                    managed_processes: 42,
                    ..
                }
            })
        ));
        server.await??;
        Ok(())
    })
    .await?
}

/// `daemon stop`/`restart` reach a daemon of another protocol with the frozen method.
#[tokio::test]
async fn connect_any_keeps_an_incompatible_daemon_for_shutdown() -> TestResult {
    timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let paths = mock_paths(&temp.path().join("home"))?;
        paths.prepare()?;
        let mut listener = listen(&paths).await?;
        let server = tokio::spawn(async move {
            let stream = listener.accept().await?;
            let mut connection = Connection::new(stream);
            connection.recv().await?;
            let mut hello = Hello::current(5);
            hello.protocol += 1;
            connection
                .send(Frame::Control(Control::Hello(hello)))
                .await?;
            let Frame::Control(Control::Request(request)) = connection.recv().await? else {
                panic!("missing shutdown")
            };
            assert_eq!(request.method, Method::DaemonShutdown);
            connection
                .send(Frame::Control(Control::Response(Response {
                    id: request.id,
                    result: Outcome::Ok {
                        data: json!({"stopping": true, "pid": 4242}),
                    },
                })))
                .await?;
            Ok::<_, devlanes_ipc::Error>(())
        });
        let mut client = Client::connect_any(&paths).await?;
        assert!(!client.compatible());
        assert_eq!(client.peer.managed_processes, 5);
        let data = client
            .request(Method::DaemonShutdown, json!({}), Context::default())
            .await?;
        assert_eq!(data["stopping"], true);
        server.await??;
        Ok(())
    })
    .await?
}

/// Only a missing socket or a refused connection means "no daemon".
#[tokio::test]
async fn not_running_is_only_missing_or_refused() -> TestResult {
    let temp = tempfile::tempdir()?;
    let paths = mock_paths(&temp.path().join("home"))?;
    let missing = Client::connect(&paths).await.err().ok_or("connected")?;
    assert!(missing.is_not_running());
    assert!(!Error::Timeout.is_not_running());
    assert!(!Error::WrongPeer.is_not_running());
    assert!(
        !Error::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)).is_not_running()
    );
    assert_eq!(
        Error::Rpc(RpcError::new("not_found", json!({}), 3)).to_string(),
        "the daemon answered not_found (exit 3)"
    );
    Ok(())
}

/// Completion calls must not create a home even when no daemon has ever run.
#[tokio::test]
async fn no_spawn_connect_has_no_filesystem_side_effects() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("missing");
    let paths = Paths::new(&home)?;
    assert!(matches!(Client::connect(&paths).await, Err(Error::Io(_))));
    assert!(!home.exists());
    Ok(())
}

/// Long homes have stable distinct socket names, and discovery itself writes nothing.
#[test]
fn long_paths_remain_isolated_and_read_only() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("a".repeat(120));
    let first = Paths::new(&home)?;
    let second = Paths::new(home.join("other"))?;
    assert_ne!(first.socket, second.socket);
    assert!(first.socket.as_os_str().len() < 104);
    assert_eq!(first.socket, Paths::new(&home)?.socket);
    assert!(!home.exists());
    Ok(())
}

/// A symlinked home cannot silently become trusted daemon state; an own home
/// others can read passes, for the daemon to tighten. The Windows version
/// (junctions and access lists) is a unit test of `paths`.
#[cfg(unix)]
#[test]
fn prepare_rejects_symlinked_homes() -> TestResult {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("shared");
    std::fs::create_dir(&home)?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755))?;
    Paths::new(&home)?.prepare()?;
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(temp.path(), &link)?;
    assert!(matches!(
        Paths::new(&link)?.prepare(),
        Err(Error::UnsafeDirectory(_))
    ));
    Ok(())
}

/// Two homes get two endpoints, each reaching only its own daemon.
#[tokio::test]
async fn two_homes_reach_two_daemons() -> TestResult {
    timeout(Duration::from_secs(10), async {
        let temp = tempfile::tempdir()?;
        let homes = [temp.path().join("one"), temp.path().join("two")];
        let mut servers = Vec::new();
        let mut all = Vec::new();
        for (managed, home) in homes.iter().enumerate() {
            let paths = mock_paths(home)?;
            paths.prepare()?;
            let mut listener = listen(&paths).await?;
            servers.push(tokio::spawn(async move {
                let mut connection = Connection::new(listener.accept().await?);
                connection.recv().await?;
                connection
                    .send(Frame::Control(Control::Hello(Hello::current(managed))))
                    .await?;
                Ok::<_, devlanes_ipc::Error>(())
            }));
            all.push(paths);
        }
        assert_ne!(all[0].socket, all[1].socket);
        for (managed, paths) in all.iter().enumerate() {
            assert_eq!(
                Client::connect(paths).await?.peer.managed_processes,
                managed
            );
        }
        for server in servers {
            server.await??;
        }
        Ok(())
    })
    .await?
}

/// Streaming consumers receive terminal response ids and can send arbitrary terminal bytes.
#[tokio::test]
async fn streaming_attach_is_bidirectional() -> TestResult {
    timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let paths = mock_paths(&temp.path().join("home"))?;
        paths.prepare()?;
        let mut listener = listen(&paths).await?;
        let server = tokio::spawn(async move {
            let stream = listener.accept().await?;
            let mut connection = Connection::new(stream);
            connection.recv().await?;
            connection.send(Frame::Control(Control::Hello(Hello::current(1)))).await?;
            let Frame::Control(Control::Request(request)) = connection.recv().await? else { panic!("missing attach") };
            assert_eq!(request.method, Method::Attach);
            connection.send(Frame::Control(Control::Response(Response { id: request.id, result: Outcome::Ok { data: json!({"stream_id":9}) } }))).await?;
            match connection.recv().await? {
                Frame::Bytes { stream_id, data } => {
                    assert_eq!(stream_id, 9);
                    assert_eq!(data.as_ref(), &[255, 0, 3]);
                },
                _ => panic!("expected terminal input"),
            }
            Ok::<_, devlanes_ipc::Error>(())
        });
        let mut client = Client::connect(&paths).await?;
        let id = client.begin_request(Method::Attach, json!({}), Context::default()).await?;
        assert!(matches!(client.next_frame().await?, Frame::Control(Control::Response(Response { id: response_id, .. })) if response_id == id));
        client.send_bytes(9, Bytes::from_static(&[255, 0, 3])).await?;
        server.await??;
        assert!(matches!(client.next_frame().await, Err(Error::Ipc(devlanes_ipc::Error::Disconnected))));
        Ok(())
    }).await?
}

/// The pointer is atomically published inside the home and fallback directories stay private.
/// Windows pipe names follow from the home, so nothing is published there.
#[cfg(unix)]
#[test]
fn prepare_writes_fallback_pointer() -> TestResult {
    let temp = tempfile::tempdir()?;
    let mut paths = Paths::new(temp.path().join("home"))?;
    paths.socket = temp.path().join("private").join("daemon.sock");
    paths.prepare()?;
    assert_eq!(
        std::fs::read_to_string(paths.run.join("daemon.socket"))?,
        format!("{}\n", paths.socket.display())
    );
    assert_eq!(
        std::fs::metadata(paths.socket.parent().ok_or("no parent")?)?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    Ok(())
}
