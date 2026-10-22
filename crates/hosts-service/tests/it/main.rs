//! The pipe server end to end, as the current user, against a temp hosts file
//! and a temp pipe name: what the service does once its checks passed, without
//! installing anything. The binary's own command line is checked too.
#![cfg(windows)]

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::Duration;

use devlanes_hosts_helper::exit;
use devlanes_hosts_helper::protocol::{
    self, DENIED, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Request, Response,
};
use devlanes_hosts_service::handler::Target;
use devlanes_hosts_service::server::{ServerConfig, read_frame, serve};
use devlanes_sys::windows::pipe::{ERROR_PIPE_BUSY, open_client};
use devlanes_sys::windows::token::{self, Sid};
use tokio::io::AsyncWriteExt as _;

/// Bound on every exchange, so a hung server fails the test instead of the run.
const TIMEOUT: Duration = Duration::from_secs(20);

fn pipe_name(tag: &str) -> String {
    format!(r"\\.\pipe\devlanes-hosts-test-{}-{tag}", std::process::id())
}

/// Sends `payload` as one frame and reads the response.
async fn exchange(name: &str, payload: &[u8]) -> Response {
    let mut client = loop {
        match open_client(OsStr::new(name)) {
            Ok(client) => break client,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("open {name}: {error}"),
        }
    };
    let framed = protocol::frame(payload, MAX_REQUEST_BYTES).expect("frame");
    client.write_all(&framed).await.expect("write");
    let response = match read_frame(&mut client, MAX_RESPONSE_BYTES).await {
        Ok(response) => response,
        Err(error) => panic!("read: {error:?}"),
    };
    Response::decode(&response).expect("response")
}

/// A running server for `user` on a fresh pipe and hosts file; stopped on drop.
struct Running {
    name: String,
    hosts: std::path::PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Running {
    async fn start(tag: &str, user: Sid) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        std::fs::write(&hosts, "127.0.0.1 localhost\r\n").expect("seed");
        let name = pipe_name(tag);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, listening) = tokio::sync::oneshot::channel::<()>();
        let config = ServerConfig {
            pipe_name: name.clone().into(),
            user,
            target: Target {
                hosts: hosts.clone(),
                flush: false,
            },
        };
        let task = tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            devlanes_hosts_service::server::serve_with(config, shutdown, move || {
                let _ = ready.send(());
            })
            .await
            .expect("serve");
        });
        tokio::time::timeout(TIMEOUT, listening)
            .await
            .expect("listening in time")
            .expect("listening");
        Self {
            name,
            hosts,
            stop: Some(stop),
            task: Some(task),
            _dir: dir,
        }
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            tokio::time::timeout(TIMEOUT, task)
                .await
                .expect("stopped in time")
                .expect("server task");
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[tokio::test]
async fn the_configured_user_applies_reads_and_clears() {
    let user = token::current_user().expect("user");
    let server = Running::start("user", user).await;
    tokio::time::timeout(TIMEOUT, async {
        let version = exchange(&server.name, &Request::Version.encode()).await;
        assert_eq!(version.code, exit::OK, "{version:?}");
        assert!(
            protocol::parse_version_line(version.message.trim()).is_some(),
            "{version:?}"
        );
        let apply = Request::Apply {
            tld: Some("test".into()),
            names: b"web.acme-2.localhost\napi.acme.test\n".to_vec(),
        };
        assert_eq!(
            exchange(&server.name, &apply.encode()).await,
            Response::ok("changed names=2\n")
        );
        let content = std::fs::read_to_string(&server.hosts).expect("read");
        assert!(
            content.contains("127.0.0.1 web.acme-2.localhost\r\n"),
            "{content}"
        );
        let status = exchange(&server.name, &Request::Status.encode()).await;
        assert!(status.message.contains("::1 api.acme.test"), "{status:?}");
        let bad = Request::Apply {
            tld: None,
            names: b"example.com\n".to_vec(),
        };
        assert_eq!(
            exchange(&server.name, &bad.encode()).await.code,
            exit::INVALID_NAMES
        );
        assert_eq!(
            exchange(&server.name, b"apply --hosts-file C:\\x\n")
                .await
                .code,
            exit::USAGE
        );
        assert_eq!(
            exchange(&server.name, &Request::Clear.encode()).await,
            Response::ok("changed names=0\n")
        );
        assert_eq!(
            std::fs::read_to_string(&server.hosts).expect("read"),
            "127.0.0.1 localhost\r\n"
        );
    })
    .await
    .expect("exchanges in time");
    server.stop().await;
}

/// Parallel clients are all answered; the writes are serialized.
#[tokio::test]
async fn parallel_clients_are_all_answered() {
    let user = token::current_user().expect("user");
    let server = Running::start("parallel", user).await;
    tokio::time::timeout(TIMEOUT, async {
        let mut tasks = Vec::new();
        for index in 0..16 {
            let name = server.name.clone();
            tasks.push(tokio::spawn(async move {
                let request = Request::Apply {
                    tld: None,
                    names: format!("n{}.localhost\n", index % 2).into_bytes(),
                };
                exchange(&name, &request.encode()).await.code
            }));
        }
        for task in tasks {
            assert_eq!(task.await.expect("client"), exit::OK);
        }
    })
    .await
    .expect("answered in time");
    server.stop().await;
}

/// A server configured for another account answers this user with `DENIED`
/// and leaves the file alone. The pipe's owner (this user) may still open it,
/// so this exercises the impersonation check rather than the access list.
#[tokio::test]
async fn other_users_are_denied() {
    let server = Running::start("denied", Sid::local_system()).await;
    tokio::time::timeout(TIMEOUT, async {
        let apply = Request::Apply {
            tld: None,
            names: b"a.localhost\n".to_vec(),
        };
        let response = exchange(&server.name, &apply.encode()).await;
        assert_eq!(response.code, DENIED, "{response:?}");
        assert_eq!(
            exchange(&server.name, &Request::Version.encode())
                .await
                .code,
            DENIED
        );
    })
    .await
    .expect("answered in time");
    assert_eq!(
        std::fs::read_to_string(&server.hosts).expect("read"),
        "127.0.0.1 localhost\r\n"
    );
    server.stop().await;
}

/// The name is taken: a second server on it fails at its first instance.
#[tokio::test]
async fn a_taken_pipe_name_is_refused() {
    let user = token::current_user().expect("user");
    let server = Running::start("taken", user.clone()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = ServerConfig {
        pipe_name: server.name.clone().into(),
        user,
        target: Target {
            hosts: dir.path().join("hosts"),
            flush: false,
        },
    };
    let result = tokio::time::timeout(TIMEOUT, serve(config, std::future::pending()))
        .await
        .expect("refused in time");
    assert!(result.is_err(), "{result:?}");
    server.stop().await;
}

/// Runs the built service binary with `args`, killed after the timeout.
fn run_binary(args: &[&str]) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devlanes-hosts-service"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let deadline = std::time::Instant::now() + TIMEOUT;
    while child.try_wait().expect("wait").is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("output");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `--version` is the only argument; anything else (a hosts path above all) is
/// refused, and without the service manager the binary says how to install it.
#[test]
fn the_binary_takes_no_paths() {
    let (code, stdout, _) = run_binary(&["--version"]);
    assert_eq!(code, Some(0));
    assert!(
        protocol::parse_version_line(stdout.trim()).is_some(),
        "{stdout}"
    );
    for args in [
        &["--hosts-file", r"C:\x"][..],
        &["apply"],
        &["--version", "extra"],
    ] {
        let (code, _, stderr) = run_binary(args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
    }
    let (code, _, stderr) = run_binary(&[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("devlanes certs trust --hosts"), "{stderr}");
}
