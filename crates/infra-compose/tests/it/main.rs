//! Docker-backed tests. Run with `cargo test -p devlanes-infra-compose -- --ignored`.
//! Each test uses a unique project prefix and removes only its own projects/containers.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    net::TcpListener,
    path::Path,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use devlanes_infra_compose::{
    Compose, ComposeCli, ComposeVersion, Error, Health, LaneInfra, LanePorts, PortBlock,
    SharedInfra, UpOptions, lane_project_name,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(240);

/// Removes test-owned compose projects and containers even when an assertion fails.
struct Cleanup {
    projects: Vec<String>,
    containers: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for project in &self.projects {
            let _ = Command::new("docker")
                .args([
                    "compose",
                    "-p",
                    project,
                    "down",
                    "--volumes",
                    "--remove-orphans",
                ])
                .current_dir("/")
                .output();
        }
        for container in &self.containers {
            let _ = Command::new("docker")
                .args(["rm", "-f", container])
                .output();
        }
    }
}

fn unique_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    format!("dltest{}x{nanos}", std::process::id())
}

/// A port that was free a moment ago; good enough for a one-port lane block in tests.
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

fn write_compose(dir: &Path, prefix: &str) {
    let compose = format!(
        r#"services:
  cache:
    image: redis:7-alpine
    ports: ["127.0.0.1::6379"]
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 1s
      retries: 30
  mail:
    image: redis:7-alpine
    container_name: {prefix}-mail
    ports: ["${{MAIL_PORT:?set by infra.env}}:6379"]
  proxy:
    image: redis:7-alpine
    container_name: {prefix}-proxy
"#
    );
    std::fs::write(dir.join("compose.yaml"), compose).expect("write compose file");
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_version_supports_override_tags() {
    bounded(async {
        let version = Compose::new(ComposeCli::new())
            .version()
            .await
            .expect("compose version");
        assert!(version >= ComposeVersion::OVERRIDE_TAGS, "{version}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_shared_and_lane_projects() {
    let prefix = unique_prefix();
    let mut cleanup = Cleanup {
        projects: vec![prefix.clone()],
        containers: Vec::new(),
    };
    for lane in [1, 2] {
        cleanup
            .projects
            .push(lane_project_name(&prefix, lane).expect("valid name"));
    }
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        write_compose(dir.path(), &prefix);
        let compose = Compose::new(ComposeCli::new());
        let env = BTreeMap::from([("MAIL_PORT".to_owned(), free_port().to_string())]);
        let shared = SharedInfra {
            project_name: Some(prefix.clone()),
            primary_dir: dir.path().to_owned(),
            files: vec!["compose.yaml".into()],
            env_file: Some(".env".into()),
            profiles: Vec::new(),
            exclude: set(&["proxy"]),
            per_lane: set(&["mail"]),
            env: env.clone(),
        };
        let report = compose
            .shared_up(&shared, UpOptions::default())
            .await
            .expect("shared up");
        assert_eq!(report.services, ["cache"]);
        assert_eq!(report.health["cache"], Health::Healthy);
        assert!(report.endpoints["cache"].port.is_some());
        assert!(report.adopted.is_empty());

        let mut lane_ports = Vec::new();
        for lane in [1, 2] {
            let infra = LaneInfra {
                project: prefix.clone(),
                lane,
                lane_dir: dir.path().to_owned(),
                files: vec!["compose.yaml".into()],
                env_file: None,
                profiles: Vec::new(),
                exclude: set(&["proxy"]),
                per_lane: set(&["mail"]),
                env: env.clone(),
                state_dir: dir.path().join(format!("state/{lane}")),
            };
            let requests = compose.lane_port_requests(&infra).await.expect("requests");
            assert_eq!(requests.len(), 1);
            let host = free_port();
            let ports = LanePorts::from([(
                "mail".to_owned(),
                BTreeMap::from([(requests[0].target, host)]),
            )]);
            let block = PortBlock::new(host, 1).expect("block");
            let report = compose
                .lane_up(&infra, &ports, block, UpOptions::default())
                .await
                .expect("lane up");
            assert_eq!(report.endpoints["mail"].port, Some(host));
            assert_eq!(report.health["mail"], Health::Running);
            assert!(report.override_path.is_some_and(|path| path.exists()));
            let status = compose.lane_status(&infra).await.expect("lane status");
            assert_eq!(status.endpoints["mail"].ports[&6379], host);
            lane_ports.push(host);
        }
        assert_ne!(lane_ports[0], lane_ports[1]);

        // The excluded service never started, and shared status still sees the cache.
        let shared_status = compose.shared_status(&shared).await.expect("shared status");
        assert_eq!(shared_status.health["cache"], Health::Healthy);
        let all = compose.cli().ps(&prefix).await.expect("ps");
        assert!(all.iter().all(|status| status.service != "proxy"));

        compose
            .lane_down(&prefix, 1, Duration::from_secs(60))
            .await
            .expect("lane 1 down");
        let lane1 = lane_project_name(&prefix, 1).expect("valid name");
        let lane2 = lane_project_name(&prefix, 2).expect("valid name");
        assert!(
            compose
                .cli()
                .ps(&lane1)
                .await
                .expect("ps lane 1")
                .is_empty()
        );
        let remaining = compose.cli().ps(&lane2).await.expect("ps lane 2");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].health(), Health::Running);
    })
    .await;
    drop(cleanup);
}

/// `(service, state)` of every container in compose project `project`.
async fn states(compose: &Compose, project: &str) -> Vec<(String, String)> {
    compose
        .cli()
        .ps(project)
        .await
        .expect("ps")
        .into_iter()
        .map(|status| (status.service, status.state))
        .collect()
}

/// Pausing a lane touches only its own project, a second call changes nothing,
/// `lane_up` right after an unpause does not wait for the next healthcheck, and
/// `lane_up` brings back a lane that is still paused.
#[tokio::test]
#[ignore = "needs docker"]
async fn docker_lane_pause_unpause() {
    let prefix = unique_prefix();
    let lane1 = lane_project_name(&prefix, 1).expect("valid name");
    let cleanup = Cleanup {
        projects: vec![prefix.clone(), lane1.clone()],
        containers: Vec::new(),
    };
    bounded(async {
        let dir = tempfile::tempdir().expect("tempdir");
        // Checks run every 500 ms until the first one passes, then every 60 s, so
        // none runs between the unpause and the `lane_up` after it.
        let file = r#"services:
  shared:
    image: redis:7-alpine
  lane:
    image: redis:7-alpine
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 60s
      start_period: 60s
      start_interval: 500ms
"#;
        std::fs::write(dir.path().join("compose.yaml"), file).expect("write compose file");
        let compose = Compose::new(ComposeCli::new());
        let shared = SharedInfra {
            project_name: Some(prefix.clone()),
            primary_dir: dir.path().to_owned(),
            files: vec!["compose.yaml".into()],
            env_file: None,
            profiles: Vec::new(),
            exclude: BTreeSet::new(),
            per_lane: set(&["lane"]),
            env: BTreeMap::new(),
        };
        compose
            .shared_up(&shared, UpOptions::default())
            .await
            .expect("shared up");
        let infra = LaneInfra {
            project: prefix.clone(),
            lane: 1,
            lane_dir: dir.path().to_owned(),
            files: vec!["compose.yaml".into()],
            env_file: None,
            profiles: Vec::new(),
            exclude: BTreeSet::new(),
            per_lane: set(&["lane"]),
            env: BTreeMap::new(),
            state_dir: dir.path().join("state"),
        };
        let block = PortBlock::new(free_port(), 1).expect("block");
        let report = compose
            .lane_up(&infra, &LanePorts::new(), block, UpOptions::default())
            .await
            .expect("lane up");
        assert_eq!(report.health["lane"], Health::Healthy);

        assert_eq!(compose.lane_pause(&prefix, 1).await.expect("pause"), 1);
        let paused = [("lane".to_owned(), "paused".to_owned())];
        assert_eq!(states(&compose, &lane1).await, paused);
        let running = [("shared".to_owned(), "running".to_owned())];
        assert_eq!(states(&compose, &prefix).await, running);
        assert_eq!(
            compose.lane_pause(&prefix, 1).await.expect("pause again"),
            0
        );
        assert_eq!(
            compose
                .lane_pause(&prefix, 9)
                .await
                .expect("no lane project"),
            0
        );

        assert_eq!(compose.lane_unpause(&prefix, 1).await.expect("unpause"), 1);
        assert_eq!(
            compose
                .lane_unpause(&prefix, 1)
                .await
                .expect("unpause again"),
            0
        );
        // Docker reads `unhealthy` until the next check, 60 s away, and `lane_up`
        // must not wait for it.
        let lane_health = |health: &str| vec![("lane".to_owned(), health.to_owned())];
        assert_eq!(health_of(&compose, &lane1).await, lane_health("unhealthy"));
        let started = std::time::Instant::now();
        let report = compose
            .lane_up(&infra, &LanePorts::new(), block, UpOptions::default())
            .await
            .expect("lane up right after the unpause");
        assert_eq!(report.health["lane"], Health::Healthy);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );

        // A lane left paused, as after a failed thaw, still comes up.
        assert_eq!(compose.lane_pause(&prefix, 1).await.expect("pause"), 1);
        let report = compose
            .lane_up(&infra, &LanePorts::new(), block, UpOptions::default())
            .await
            .expect("lane up while paused");
        assert_eq!(report.health["lane"], Health::Healthy);
        let running = [("lane".to_owned(), "running".to_owned())];
        assert_eq!(states(&compose, &lane1).await, running);
    })
    .await;
    drop(cleanup);
}

/// `(service, health)` of every container in compose project `project`.
async fn health_of(compose: &Compose, project: &str) -> Vec<(String, String)> {
    compose
        .cli()
        .ps(project)
        .await
        .expect("ps")
        .into_iter()
        .map(|status| (status.service, status.health))
        .collect()
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_container_name_conflict_is_explained() {
    let prefix = unique_prefix();
    let foreign = format!("{prefix}-mail");
    let cleanup = Cleanup {
        projects: vec![prefix.clone()],
        containers: vec![foreign.clone()],
    };
    bounded(async {
        let created = Command::new("docker")
            .args(["create", "--name", &foreign, "redis:7-alpine"])
            .output()
            .expect("docker create");
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let dir = tempfile::tempdir().expect("tempdir");
        write_compose(dir.path(), &prefix);
        let shared = SharedInfra {
            project_name: Some(prefix.clone()),
            primary_dir: dir.path().to_owned(),
            files: vec!["compose.yaml".into()],
            env_file: None,
            profiles: Vec::new(),
            exclude: set(&["proxy", "cache"]),
            per_lane: BTreeSet::new(),
            env: BTreeMap::from([("MAIL_PORT".to_owned(), free_port().to_string())]),
        };
        let result = Compose::new(ComposeCli::new())
            .shared_up(&shared, UpOptions::default())
            .await;
        match result {
            Err(Error::ContainerNameConflict(conflicts)) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].container_name, foreign);
                assert_eq!(conflicts[0].owner, None);
            }
            other => panic!("expected a name conflict, got {other:?}"),
        }
    })
    .await;
    drop(cleanup);
}
