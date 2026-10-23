//! The pipe server as the current user, against a temp hosts file and a temp
//! pipe name, without installing anything; the binary's own command line too.
//! The daemon's client against this server is tested in
//! `devlanes-resolver-hosts` (`tests/it/service.rs`).
#![cfg(windows)]

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::Duration;

use devlanes_hosts_helper::exit;
use devlanes_hosts_helper::protocol::{
    self, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Request, Response,
};
use devlanes_hosts_service::handler::Target;
use devlanes_hosts_service::server::{FrameError, ServerConfig, read_frame, serve};
use devlanes_sys::windows::pipe::{ERROR_PIPE_BUSY, create_server, open_client};
use devlanes_sys::windows::token::{self, Sid};
use tokio::io::AsyncWriteExt as _;
use tokio::net::windows::named_pipe::NamedPipeClient;

/// Bound on every exchange, so a hung server fails the test instead of the run.
const TIMEOUT: Duration = Duration::from_secs(20);

fn pipe_name(tag: &str) -> String {
    format!(r"\\.\pipe\devlanes-hosts-test-{}-{tag}", std::process::id())
}

/// Opens the pipe, waiting while every instance is busy.
async fn connect(name: &str) -> NamedPipeClient {
    loop {
        match open_client(OsStr::new(name)) {
            Ok(client) => return client,
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("open {name}: {error}"),
        }
    }
}

/// Sends `payload` as one frame and reads the response.
async fn exchange(name: &str, payload: &[u8]) -> Response {
    let mut client = connect(name).await;
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

/// Parallel clients are all answered, one at a time; a malformed request gets
/// the usage code.
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
        assert_eq!(
            exchange(&server.name, b"apply --hosts-file C:\\x\n")
                .await
                .code,
            exit::USAGE
        );
    })
    .await
    .expect("answered in time");
    let content = std::fs::read_to_string(&server.hosts).expect("read");
    assert!(content.contains(".localhost\r\n"), "{content}");
    server.stop().await;
}

/// The name is taken: a second server on it fails at its first instance once
/// its wait for the name runs out.
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

/// A client handle left over from a stopped server keeps the name alive, as
/// after a service restart or reinstall; the next server waits for it to close
/// instead of failing.
#[tokio::test]
async fn a_new_server_waits_for_a_leftover_client() {
    let user = token::current_user().expect("user");
    let server = Running::start("leftover", user.clone()).await;
    let name = server.name.clone();
    let client = tokio::time::timeout(TIMEOUT, connect(&name))
        .await
        .expect("connected in time");
    server.stop().await;
    assert!(
        create_server(OsStr::new(&name), true).is_err(),
        "the client keeps the name"
    );
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(client);
    });
    let server = Running::start("leftover", user).await;
    release.await.expect("release");
    let response = tokio::time::timeout(TIMEOUT, exchange(&server.name, b"version\n"))
        .await
        .expect("answered in time");
    assert_eq!(response.code, exit::OK);
    server.stop().await;
}

/// A frame announcing far more than it sends fails as a short read, holding
/// only what arrived.
#[tokio::test]
async fn short_frames_fail_without_the_announced_allocation() {
    let mut framed = u32::try_from(MAX_REQUEST_BYTES)
        .expect("limit fits")
        .to_le_bytes()
        .to_vec();
    framed.extend_from_slice(b"version\n");
    let mut reader = framed.as_slice();
    let error = read_frame(&mut reader, MAX_REQUEST_BYTES)
        .await
        .expect_err("short");
    assert!(
        matches!(&error, FrameError::Io(error) if error.kind() == std::io::ErrorKind::UnexpectedEof),
        "{error:?}"
    );
    let whole = protocol::frame(b"version\n", MAX_REQUEST_BYTES).expect("frame");
    let mut reader = whole.as_slice();
    assert_eq!(
        read_frame(&mut reader, MAX_REQUEST_BYTES)
            .await
            .expect("whole"),
        b"version\n"
    );
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

/// `--version` and `--user-sid <SID>` are the only arguments; anything else (a
/// hosts path above all) is refused, and without the service manager the
/// binary says how to install it.
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
        &["--user-sid", "SY"],
        &["--user-sid", "S-1-5-21-1-2-3-1001", "--hosts-file", r"C:\x"],
        &[],
        &["--user-sid", "S-1-5-21-1-2-3-1001"],
    ] {
        let (code, _, stderr) = run_binary(args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains("devlanes certs trust --hosts"), "{stderr}");
    }
}
