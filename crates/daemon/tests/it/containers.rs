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

#[tokio::test(flavor = "multi_thread")]
/// Shared containers show in every lane of the project, idle ones included,
/// with `hidden` from `infra.hidden`; a lane's own containers only in that lane.
async fn shared_containers_show_in_every_lane() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = CONFIG.replace(
            "infra:\n  containers:\n",
            "infra:\n  hidden: [cache]\n  containers:\n    cache: { image: \"redis:7\", ports: { \"6379\": 6379 } }\n",
        );
        let main = repo(&tmp.path, "r", &config);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        h.call(Method::LaneNew, json!({"name": "idle"}), cx(&main))
            .await
            .expect("second lane");
        let status = h
            .call(Method::Status, json!({}), cx(&main))
            .await
            .expect("status");
        let lanes: Page<LaneSummary> = serde_json::from_value(status).expect("page");
        let names = |reference: &str| -> Vec<(String, bool, bool)> {
            let lane = lanes
                .items
                .iter()
                .find(|lane| lane.reference == reference)
                .expect(reference);
            let mut names: Vec<_> = lane
                .containers
                .iter()
                .map(|c| (c.name.clone(), c.shared, c.hidden))
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            names("demo@0"),
            [("cache".into(), true, true), ("db".into(), false, false)]
        );
        assert_eq!(names("demo@1"), [("cache".into(), true, true)]);
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// `ContainerSummary.state` of the lane's `db` container.
async fn db_state(h: &crate::Harness, main: &std::path::Path) -> Option<String> {
    let status = h.call(Method::Status, json!({}), cx(main)).await.ok()?;
    let lanes: Page<LaneSummary> = serde_json::from_value(status).ok()?;
    lanes.items[0]
        .containers
        .iter()
        .find(|c| c.name == "db")
        .and_then(|c| c.state.clone())
}

/// A machine at 37% CPU with 4 GiB of 16 GiB available, under warn pressure.
struct FakeMachine;

#[async_trait]
impl devlanes_core::gate::PressureSource for FakeMachine {
    fn name(&self) -> &'static str {
        "fake"
    }
    async fn sample(&self) -> Result<devlanes_core::gate::PressureSample, devlanes_core::Error> {
        Ok(devlanes_core::gate::PressureSample {
            cpu_percent: Some(37.0),
            memory: Some(devlanes_core::gate::MemoryPressure::Warn),
            cpu_stall_percent: None,
        })
    }
    async fn available_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
        Ok(Some(4 << 30))
    }
    async fn total_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
        Ok(Some(16 << 30))
    }
}

#[tokio::test(flavor = "multi_thread")]
/// `metrics` sends the machine, lane totals with peaks since the start, the
/// lanes of each container, windows and bucketed histories on request; a
/// container event changes `ContainerSummary.state`.
async fn metrics_carry_system_lanes_and_container_state() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                metrics: vec![Arc::new(FakeDocker::default())],
                system: Some(Arc::new(FakeMachine)),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let metrics = h
            .call(
                Method::Metrics,
                json!({"range_ms": 3_600_000, "points": 30}),
                cx(&main),
            )
            .await
            .expect("metrics");
        assert_eq!(metrics["range_ms"], 3_600_000);
        let system = &metrics["system"];
        assert_eq!(system["cpu_percent"], 37.0, "{system}");
        assert_eq!(system["memory_pressure"], "warn");
        assert_eq!(system["available_memory_bytes"], 4_u64 << 30);
        assert_eq!(system["memory_total_bytes"], 16_u64 << 30);
        assert!(system["cpus"].as_u64().is_some_and(|cpus| cpus > 0));
        assert!(!system["history"].as_array().expect("history").is_empty());

        let lane = metrics["lanes"]
            .as_array()
            .expect("lanes")
            .iter()
            .find(|lane| lane["lane"] == "demo@0")
            .expect("lane total");
        assert_eq!(
            lane["memory_bytes"], 1000,
            "per-lane containers count: {lane}"
        );
        assert_eq!(lane["peak_memory_bytes"], 1000, "{lane}");
        assert!(
            lane["windows"].is_array(),
            "past 15 minutes windows come along: {lane}"
        );

        let rows = metrics["units"].as_array().expect("units");
        let cache = rows
            .iter()
            .find(|row| row["unit"] == "cache")
            .expect("cache");
        assert_eq!(cache["lanes"], json!(["demo@0"]));
        let api = rows.iter().find(|row| row["unit"] == "api").expect("api");
        assert_eq!(
            (&api["kind"], &api["role"]),
            (&json!("service"), &json!("service"))
        );

        let short = h
            .call(Method::Metrics, json!({}), cx(&main))
            .await
            .expect("metrics");
        assert_eq!(short["range_ms"], 900_000, "the default is the raw ring");
        assert!(short["lanes"][0].get("windows").is_none());

        assert_eq!(db_state(&h, &main).await.as_deref(), Some("healthy"));
        h.infra.emit(devlanes_daemon::integrations::InfraEvent {
            project: "demo".into(),
            lane: Some(0),
            container: "demo-lane0-db-1".into(),
            state: "died".into(),
            exit_code: Some(137),
        });
        let exited = crate::eventually(std::time::Duration::from_secs(5), || async {
            db_state(&h, &main)
                .await
                .filter(|state| state == "exited 137")
        })
        .await;
        assert!(exited.is_some(), "the event reaches the summary");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}
