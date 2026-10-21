//! Docker stats for a lane's infra containers reach `status` and `metrics`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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
