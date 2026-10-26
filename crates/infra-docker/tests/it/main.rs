//! Docker-backed tests. Run with `cargo test -p devlanes-infra-docker -- --ignored`.
//! Each test uses a unique project label and removes only resources carrying it.

use std::{
    collections::BTreeMap,
    future::Future,
    net::TcpListener,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use devlanes_infra_docker::{
    ContainerSpec, DockerInfra, EnsureAction, EnsureOptions, Error, EventKind, Health, LogChunk,
    LogOptions, LogStream, NamedPort, PortBlock, Protocol, Scope, container_name, labels,
};
use futures_util::StreamExt;

const TEST_TIMEOUT: Duration = Duration::from_secs(180);
const IMAGE: &str = "redis:7-alpine";

/// Removes containers and volumes labelled with the test project, plus extra names.
struct Cleanup {
    project: String,
    names: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let filter = format!("label={}={}", labels::PROJECT, self.project);
        let listed = |args: &[&str]| -> Vec<String> {
            Command::new("docker")
                .args(args)
                .output()
                .map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut containers = listed(&["ps", "-aq", "--filter", &filter]);
        containers.extend(self.names.iter().cloned());
        for container in containers {
            let _ = Command::new("docker")
                .args(["rm", "-fv", &container])
                .output();
        }
        for volume in listed(&["volume", "ls", "-q", "--filter", &filter]) {
            let _ = Command::new("docker")
                .args(["volume", "rm", "-f", &volume])
                .output();
        }
    }
}

fn unique_project() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    format!("dltest{}x{nanos}", std::process::id())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("free port")
}

async fn bounded<F: Future<Output = ()>>(test: F) {
    tokio::time::timeout(TEST_TIMEOUT, test)
        .await
        .expect("test finished in time");
}

fn redis_spec(
    project: &str,
    name: &str,
    scope: Scope,
    base_dir: &std::path::Path,
) -> ContainerSpec {
    let host = free_port();
    ContainerSpec {
        project: project.to_owned(),
        name: name.to_owned(),
        image: IMAGE.to_owned(),
        command: None,
        env: BTreeMap::new(),
        volumes: vec!["data:/data".to_owned(), "./conf:/conf:ro".to_owned()],
        ports: vec![NamedPort {
            name: "redis".to_owned(),
            container: 6379,
            host,
            protocol: Protocol::Tcp,
        }],
        scope,
        base_dir: base_dir.to_owned(),
        block: PortBlock::new(host, 1).expect("block"),
    }
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_lane_container_lifecycle() {
    let project = unique_project();
    let foreign = container_name(&project, "foreign", Scope::Shared);
    let cleanup = Cleanup {
        project: project.clone(),
        names: vec![foreign.clone()],
    };
    bounded(async {
        let base = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(base.path().join("conf")).expect("mkdir");
        std::fs::write(base.path().join("conf/marker"), "lane").expect("write");
        let infra = DockerInfra::connect().await.expect("connect");
        assert!(infra.server_version().await.expect("version").is_some());
        let options = EnsureOptions::default();

        let lane = redis_spec(&project, "cache", Scope::Lane(1), base.path());
        let created = infra.ensure(&lane, options).await.expect("create");
        assert_eq!(created.action, EnsureAction::Created);
        assert_eq!(created.health, Health::Healthy);
        assert_eq!(created.endpoint.port, Some(lane.ports[0].host));
        assert_eq!(created.endpoint.ports["redis"], lane.ports[0].host);
        let marker = Command::new("docker")
            .args(["exec", &created.container_name, "cat", "/conf/marker"])
            .output()
            .expect("docker exec");
        assert_eq!(
            String::from_utf8_lossy(&marker.stdout),
            "lane",
            "relative bind mount"
        );

        let adopted = infra.ensure(&lane, options).await.expect("adopt");
        assert_eq!(adopted.action, EnsureAction::Adopted);
        assert_eq!(adopted.container_id, created.container_id);

        let mut changed = lane.clone();
        changed.env.insert("CHANGED".into(), "1".into());
        let recreated = infra.ensure(&changed, options).await.expect("recreate");
        assert_eq!(recreated.action, EnsureAction::Recreated);
        assert_ne!(recreated.container_id, created.container_id);

        infra
            .stop(&project, Some(Scope::Lane(1)), Duration::from_secs(5))
            .await
            .expect("stop");
        let restarted = infra.ensure(&changed, options).await.expect("start");
        assert_eq!(restarted.action, EnsureAction::Started);

        let shared = redis_spec(&project, "shared-cache", Scope::Shared, base.path());
        infra.ensure(&shared, options).await.expect("shared");
        let managed = infra.list_managed(&project).await.expect("list");
        assert_eq!(managed.len(), 2);
        let lane_entry = managed
            .iter()
            .find(|container| container.scope == Some(Scope::Lane(1)))
            .expect("lane container listed");
        assert_eq!(lane_entry.key.as_deref(), Some("cache"));
        assert_eq!(lane_entry.ports[&6379], lane.ports[0].host);

        let removed = infra.remove_lane(&project, 1).await.expect("remove lane");
        assert_eq!(
            removed.containers,
            [container_name(&project, "cache", Scope::Lane(1))]
        );
        assert_eq!(removed.volumes.len(), 1);
        let remaining = infra.list_managed(&project).await.expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope, Some(Scope::Shared));

        let created = Command::new("docker")
            .args(["create", "--name", &foreign, IMAGE])
            .output()
            .expect("docker create");
        assert!(created.status.success());
        let conflict = infra
            .ensure(
                &redis_spec(&project, "foreign", Scope::Shared, base.path()),
                options,
            )
            .await;
        assert!(
            matches!(&conflict, Err(Error::NameConflict { owner: None, .. })),
            "{conflict:?}"
        );
    })
    .await;
    drop(cleanup);
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_events_report_lifecycle() {
    let project = unique_project();
    let cleanup = Cleanup {
        project: project.clone(),
        names: Vec::new(),
    };
    bounded(async {
        let base = tempfile::tempdir().expect("tempdir");
        let infra = DockerInfra::connect().await.expect("connect");
        let mut events = infra.project_events(&project);
        let mut spec = redis_spec(&project, "cache", Scope::Lane(3), base.path());
        spec.volumes.clear();
        infra
            .ensure(&spec, EnsureOptions::default())
            .await
            .expect("ensure");
        infra
            .stop(&project, None, Duration::from_secs(5))
            .await
            .expect("stop");
        let mut kinds = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_secs(20), events.next()).await
        {
            let event = event.expect("event");
            assert_eq!(event.project(), Some(project.as_str()));
            assert_eq!(event.scope(), Some(Scope::Lane(3)));
            kinds.push(event.kind);
            // Docker's order of `stop` and `die` differs between engines; wait for both.
            let died = kinds
                .iter()
                .any(|kind| matches!(kind, EventKind::Died { .. }));
            if died && kinds.contains(&EventKind::Stopped) {
                break;
            }
        }
        assert_eq!(kinds.first(), Some(&EventKind::Started), "{kinds:?}");
        assert!(
            kinds
                .iter()
                .any(|kind| matches!(kind, EventKind::Died { .. })),
            "{kinds:?}"
        );
        assert!(kinds.contains(&EventKind::Stopped), "{kinds:?}");
    })
    .await;
    drop(cleanup);
}

/// Concatenated bytes of `stream` chunks.
fn text_of(chunks: &[LogChunk], stream: LogStream) -> String {
    chunks
        .iter()
        .filter(|chunk| chunk.stream == stream)
        .map(|chunk| String::from_utf8_lossy(&chunk.bytes).into_owned())
        .collect()
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_logs_tail_and_follow() {
    let project = unique_project();
    let name = format!("{project}-logs");
    let cleanup = Cleanup {
        project: project.clone(),
        names: vec![name.clone()],
    };
    bounded(async {
        let script = "echo one; echo two >&2; echo three; sleep 1; echo four; sleep 300";
        let created = Command::new("docker")
            .args(["run", "-d", "--name", &name, IMAGE, "sh", "-c", script])
            .output()
            .expect("docker run");
        assert!(created.status.success(), "{created:?}");
        let infra = DockerInfra::connect().await.expect("connect");

        let mut follow = infra.logs(
            &name,
            LogOptions {
                follow: true,
                ..LogOptions::default()
            },
        );
        let mut followed = Vec::new();
        while !text_of(&followed, LogStream::Stdout).contains("four") {
            let chunk = tokio::time::timeout(Duration::from_secs(30), follow.next())
                .await
                .expect("output in time")
                .expect("stream open")
                .expect("chunk");
            followed.push(chunk);
        }
        assert_eq!(text_of(&followed, LogStream::Stdout), "one\nthree\nfour\n");
        assert_eq!(text_of(&followed, LogStream::Stderr), "two\n");

        let tail: Vec<LogChunk> = infra
            .logs(
                &name,
                LogOptions {
                    tail: Some(1),
                    timestamps: true,
                    ..LogOptions::default()
                },
            )
            .map(|chunk| chunk.expect("chunk"))
            .collect()
            .await;
        let last = text_of(&tail, LogStream::Stdout);
        let (timestamp, line) = last.split_once(' ').expect("timestamped");
        assert!(timestamp.ends_with('Z'), "{last}");
        assert_eq!(line, "four\n");

        let missing = infra
            .logs(&format!("{name}-missing"), LogOptions::default())
            .next()
            .await
            .expect("an error");
        assert!(
            missing.as_ref().is_err_and(Error::is_not_found),
            "{missing:?}"
        );
    })
    .await;
    drop(cleanup);
}
