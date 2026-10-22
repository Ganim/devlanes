//! The Windows pipe client against the hosts service's own server code, run as
//! the current user on a temp pipe and a temp hosts file. The real install is
//! an ignored test: it needs an elevated terminal and changes the machine.

use std::collections::BTreeSet;
use std::net::ToSocketAddrs as _;
use std::path::PathBuf;
use std::time::Duration;

use devlanes_hosts_service::handler::Target;
use devlanes_hosts_service::server::{ServerConfig, serve_with};
use devlanes_resolver_hosts::pipe::PipeHostsClient;
use devlanes_resolver_hosts::{Applied, HostsClient, HostsError};
use devlanes_sys::windows::token::{self, Sid};

/// Bound on each test's exchanges.
const LIMIT: Duration = Duration::from_secs(30);

/// A server on a fresh pipe answering `user`, with its hosts file; stopped on drop.
struct Server {
    name: String,
    hosts: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start(tag: &str, user: Sid) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        std::fs::write(&hosts, "127.0.0.1 localhost\r\n").expect("seed");
        let name = format!(
            r"\\.\pipe\devlanes-resolver-hosts-test-{}-{tag}",
            std::process::id()
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, listening) = tokio::sync::oneshot::channel::<()>();
        let config = ServerConfig {
            pipe_name: name.clone().into(),
            user,
            target: Target {
                hosts: hosts.clone(),
                flush: false,
                check_directory: false,
            },
        };
        let task = tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            serve_with(config, shutdown, move || {
                let _ = ready.send(());
            })
            .await
            .expect("serve");
        });
        tokio::time::timeout(LIMIT, listening)
            .await
            .expect("listening in time")
            .expect("listening");
        Self {
            name,
            hosts,
            stop: Some(stop),
            task,
            _dir: dir,
        }
    }

    fn content(&self) -> String {
        std::fs::read_to_string(&self.hosts).expect("read hosts")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

/// With the server's account as the expected owner (the current user here,
/// SYSTEM for the real service), every operation works like the sudo client.
#[tokio::test]
async fn the_pipe_client_applies_reads_and_clears() {
    let user = token::current_user().expect("user");
    let server = Server::start("ok", user.clone()).await;
    let client = HostsClient::service(PipeHostsClient::new(&server.name, user), Some("test"))
        .expect("client");
    tokio::time::timeout(LIMIT, async {
        let version = client.version().await.expect("version");
        assert_eq!(version.version, env!("CARGO_PKG_VERSION"));
        let applied = client
            .apply(&names(&["web.acme-2.localhost", "api.acme.test"]))
            .await
            .expect("apply");
        assert_eq!(
            applied,
            Applied {
                changed: true,
                names: 2
            }
        );
        assert!(server.content().contains("::1 api.acme.test\r\n"));
        let status = client.status().await.expect("status");
        assert!(
            status.contains("127.0.0.1 web.acme-2.localhost"),
            "{status}"
        );
        // Filtered before anything is sent.
        let error = client
            .apply(&names(&["example.com"]))
            .await
            .expect_err("invalid");
        assert_eq!(error.code(), "hosts_invalid_names");
        let cleared = client.clear().await.expect("clear");
        assert_eq!(
            cleared,
            Applied {
                changed: true,
                names: 0
            }
        );
        assert_eq!(server.content(), "127.0.0.1 localhost\r\n");
    })
    .await
    .expect("in time");
}

/// The real client only talks to a pipe owned by SYSTEM. A pipe this user
/// created (as a squatter would) is refused before any request is sent.
#[tokio::test]
async fn a_pipe_not_owned_by_system_is_refused() {
    let user = token::current_user().expect("user");
    let server = Server::start("squatted", user).await;
    let client = HostsClient::service(
        PipeHostsClient::new(&server.name, Sid::local_system()),
        None,
    )
    .expect("client");
    tokio::time::timeout(LIMIT, async {
        let error = client
            .apply(&names(&["a.localhost"]))
            .await
            .expect_err("untrusted");
        assert!(
            matches!(error, HostsError::ServiceUntrusted { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), "service_untrusted");
        assert!(error.is_denied());
        assert_eq!(
            client.version().await.expect_err("untrusted").code(),
            "service_untrusted"
        );
    })
    .await
    .expect("in time");
    assert_eq!(server.content(), "127.0.0.1 localhost\r\n");
}

/// A service installed for another account answers this one with
/// `service_denied`.
#[tokio::test]
async fn another_users_service_denies() {
    let user = token::current_user().expect("user");
    let server = Server::start("denied", Sid::local_system()).await;
    let client =
        HostsClient::service(PipeHostsClient::new(&server.name, user), None).expect("client");
    let error = tokio::time::timeout(LIMIT, client.apply(&names(&["a.localhost"])))
        .await
        .expect("in time")
        .expect_err("denied");
    assert_eq!(error.code(), "service_denied", "{error:?}");
    assert_eq!(server.content(), "127.0.0.1 localhost\r\n");
}

/// The real thing: installs the service from `DEVLANES_HOSTS_SERVICE`, writes a
/// name, resolves it through the system resolver, clears and uninstalls. Run
/// from an elevated terminal:
///
/// ```text
/// cargo build --release -p devlanes-hosts-service
/// set DEVLANES_HOSTS_SERVICE=%CD%\target\release\devlanes-hosts-service.exe
/// cargo test -p devlanes-resolver-hosts --test it -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs an elevated terminal; installs the real service"]
async fn installs_applies_and_uninstalls_the_service() {
    let source = PathBuf::from(
        std::env::var_os("DEVLANES_HOSTS_SERVICE").expect("set DEVLANES_HOSTS_SERVICE"),
    );
    let user = token::current_user().expect("user");
    let run = async {
        let installed = tokio::task::spawn_blocking(move || {
            devlanes_hosts_service::install::install(&source, &user, &|| Ok(()))
        })
        .await
        .expect("join")
        .expect("install");
        assert!(installed.path.ends_with("devlanes-hosts-service.exe"));
        let client = HostsClient::service(PipeHostsClient::system(), None).expect("client");
        client.version().await.expect("version over the pipe");
        let name = format!("devlanes-it-{}.localhost", std::process::id());
        let applied = client.apply(&names(&[&name])).await.expect("apply");
        assert_eq!(applied.names, 1);
        let lookup = name.clone();
        let addrs = tokio::task::spawn_blocking(move || {
            (lookup.as_str(), 0)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>)
        })
        .await
        .expect("join")
        .expect("resolves");
        assert!(
            addrs.iter().all(|addr| addr.ip().is_loopback()),
            "{addrs:?}"
        );
        client.clear().await.expect("clear");
        let warnings = tokio::task::spawn_blocking(devlanes_hosts_service::install::uninstall)
            .await
            .expect("join")
            .expect("uninstall");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            client.version().await.expect_err("gone").code(),
            "service_missing"
        );
    };
    tokio::time::timeout(Duration::from_secs(180), run)
        .await
        .expect("finishes in time");
}
