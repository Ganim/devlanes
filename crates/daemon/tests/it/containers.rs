//! Infra containers: Docker stats reach `status` and `metrics`, `ready` checks
//! gate the infra phase, and `infra.hosts` routes reach the proxy.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use devlanes_daemon::integrations::{RouteEntry, RouteSink};
use devlanes_metrics::{Error, MetricsSource, Sample, SampleRequest, Subject, unix_ms};
use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, harness, repo, temp};

const CONFIG: &str = r#"project: demo
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
services:
  api: { run: sleep }
"#;

/// Reports 2.5% CPU and 1000 bytes for every requested container and keeps the
/// requests it saw.
#[derive(Default)]
struct FakeDocker {
    requests: Mutex<Vec<SampleRequest>>,
}

#[async_trait]
impl MetricsSource for FakeDocker {
    /// `docker`, like the real collector.
    fn name(&self) -> &'static str {
        "docker"
    }

    /// One fixed reading per container target.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        let ts_ms = unix_ms(std::time::SystemTime::now());
        Ok(request
            .containers
            .iter()
            .map(|target| Sample {
                subject: Subject::Container {
                    project: target.project,
                    lane: target.lane,
                    id: target.id.clone(),
                },
                cpu_percent: 2.5,
                footprint_bytes: 1000,
                rss_bytes: 1000,
                ts_ms,
            })
            .collect())
    }
}

/// Keeps the last published route snapshot.
#[derive(Default)]
struct Recorded(Mutex<Vec<RouteEntry>>);

impl RouteSink for Recorded {
    fn publish(&self, routes: Vec<RouteEntry>) {
        if let Ok(mut last) = self.0.lock() {
            *last = routes;
        }
    }
}

/// A lane whose `db` container (fake infra: published as `5432`) has `ready`.
fn ready_config(ready: &str) -> String {
    format!(
        "project: demo\ninfra:\n  containers:\n    db: {{ image: \"postgres:16\", ports: {{ \"5432\": 5432 }}, per_lane: true, ready: {ready} }}\nservices:\n  api: {{ run: sleep, ready: started }}\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
/// A container's `ready` gates the infra phase: a passing `cmd` or `log` check
/// lets services start, a failing one fails the start with
/// `container_not_ready` before any service spawns.
async fn container_ready_gates_the_infra_phase() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        for (name, ready, passes) in [
            ("cmd-ok", r#"{ cmd: "true" }"#, true),
            ("cmd-fails", r#"{ cmd: "false", timeout: 1s }"#, false),
            ("log", r#"{ log: "ready to accept", timeout: 5s }"#, true),
            (
                "log-missing",
                r#"{ log: "never printed", timeout: 1s }"#,
                false,
            ),
        ] {
            let main = repo(&tmp.path, name, &ready_config(ready));
            let h = harness(
                &tmp.path.join(format!("home-{name}")),
                Setup {
                    infra: true,
                    ..Setup::default()
                },
            )
            .await;
            *h.infra.logs.lock().expect("logs") =
                vec!["database system is ready to accept connections".into()];
            let started = h.call(Method::Start, json!({}), cx(&main)).await;
            if passes {
                let started = started.expect("start");
                assert_eq!(started["ready"], true, "{name}: {started}");
            } else {
                let error = started.expect_err(name);
                assert_eq!(error.code, "container_not_ready", "{name}: {error:?}");
                assert_eq!(error.data["container"], "db");
                assert!(
                    h.runner.journal.entries().is_empty(),
                    "{name}: no service spawned"
                );
            }
            h.call(Method::Stop, json!({}), cx(&main))
                .await
                .expect("stop");
            h.close().await;
        }
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A missing `infra.compose` file fails `start` with the located
/// `config_error` that `config check` reports, before the infra provider runs.
async fn a_missing_compose_file_fails_the_start_before_docker() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: demo\ninfra:\n  compose: missing.yml\nservices:\n  api: { run: sleep, ready: started }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let check = h
            .call(Method::ConfigCheck, json!({}), cx(&main))
            .await
            .expect_err("config check");
        let error = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect_err("start");
        assert_eq!(error.code, "config_error", "{error:?}");
        assert_eq!(error.data["message"], check.data["message"], "{error:?}");
        assert_eq!(error.data["line"], 3, "{error:?}");
        assert!(h.infra.calls.entries().is_empty(), "docker was not reached");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `infra.hosts` object entries route to the named published port with their
/// `fallback` and `host_header`, like service hosts.
async fn infra_host_objects_keep_port_fallback_and_host_header() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = r#"project: demo
infra:
  containers:
    db: { image: "postgres:16", ports: { "5432": 5432 }, per_lane: true }
  hosts:
    db:
      - "db.${{ lane.domain }}"
      - { host: "*.db.${{ lane.domain }}", port: "5432", fallback: true, host_header: upstream }
services:
  api: { run: sleep, ready: started }
"#;
        let main = repo(&tmp.path, "r", config);
        let routes = Arc::new(Recorded::default());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                routes: Some(routes.clone()),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let published = routes.0.lock().expect("routes").clone();
        let plain = published
            .iter()
            .find(|r| r.host == "db.demo.localhost")
            .expect("plain route");
        assert!(!plain.fallback && !plain.host_header_upstream);
        let wildcard = published
            .iter()
            .find(|r| r.host == "*.db.demo.localhost")
            .expect("object route");
        assert!(
            wildcard.fallback && wildcard.host_header_upstream,
            "{wildcard:?}"
        );
        assert_eq!(wildcard.port, plain.port);
        assert_eq!(wildcard.unit, "db");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// The sampler asks for the lane's own container with its lane and the shared one
/// without; `status` shows the readings on the lane's containers and `metrics`
/// lists them under the lane or `<project> (shared)`.
async fn infra_containers_are_sampled() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let docker = Arc::new(FakeDocker::default());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                metrics: vec![docker.clone()],
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");

        let metrics = h
            .call(Method::Metrics, json!({}), cx(&main))
            .await
            .expect("metrics");
        let request = docker
            .requests
            .lock()
            .ok()
            .and_then(|requests| requests.last().cloned())
            .expect("sampled");
        let mut targets: Vec<(String, bool)> = request
            .containers
            .iter()
            .map(|target| (target.id.clone(), target.lane.is_some()))
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            [
                ("demo-cache-1".to_owned(), false),
                ("demo-lane0-db-1".to_owned(), true)
            ]
        );
        let mut containers: Vec<(&str, &str)> = metrics["units"]
            .as_array()
            .expect("units")
            .iter()
            .filter(|unit| unit["kind"] == "container")
            .filter_map(|unit| Some((unit["lane"].as_str()?, unit["unit"].as_str()?)))
            .collect();
        containers.sort_unstable();
        assert_eq!(containers, [("demo (shared)", "cache"), ("demo@0", "db")]);

        let status = h
            .call(Method::Status, json!({}), cx(&main))
            .await
            .expect("status");
        let lanes: Page<LaneSummary> = serde_json::from_value(status).expect("page");
        let db = lanes.items[0]
            .containers
            .iter()
            .find(|container| container.name == "db")
            .expect("db");
        assert_eq!((db.cpu_percent, db.memory_bytes), (Some(2.5), Some(1000)));
        assert!(!db.shared);
        let json: Value = serde_json::to_value(db).expect("json");
        assert_eq!(json["memory_bytes"], 1000);

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}
