//! Unit metrics: every running unit is sampled, in the extra lanes of a folder
//! too, its history grows with each tick, and a unit that yields no reading
//! shows none instead of its last one.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use devlanes_metrics::{Error, MetricsSource, Sample, SampleRequest, Subject, unix_ms};
use devlanes_proto::{Context, LaneSummary, Method, Page};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

const CONFIG: &str = "project: demo\nservices:\n  api: { run: sleep, ready: started }\n";

/// Reports each unit's leader at 3% CPU and 1 MiB while `reporting`, and
/// nothing otherwise, like a collector that finds none of a unit's processes.
struct Switchable {
    reporting: AtomicBool,
}

#[async_trait]
impl MetricsSource for Switchable {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        if !self.reporting.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let ts_ms = unix_ms(std::time::SystemTime::now());
        Ok(request
            .units
            .iter()
            .filter_map(|target| {
                let leader = target.processes.first()?;
                Some(Sample {
                    subject: Subject::Process {
                        unit: target.unit.clone(),
                        pid: leader.pid,
                        start_time: leader.start_time,
                    },
                    cpu_percent: 3.0,
                    footprint_bytes: 1 << 20,
                    rss_bytes: 1 << 20,
                    ts_ms,
                })
            })
            .collect())
    }
}

/// `(lane, cpu_percent, memory_bytes, history length)` of every `api` row.
async fn api_rows(h: &crate::Harness, context: Context) -> Vec<(String, Value, Value, usize)> {
    let metrics = h
        .call(Method::Metrics, json!({"range_ms": 120_000}), context)
        .await
        .expect("metrics");
    let mut rows: Vec<_> = metrics["units"]
        .as_array()
        .expect("units")
        .iter()
        .filter(|row| row["unit"] == "api")
        .map(|row| {
            (
                row["lane"].as_str().unwrap_or_default().to_owned(),
                row["cpu_percent"].clone(),
                row["memory_bytes"].clone(),
                row["history"].as_array().map_or(0, Vec::len),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// `cpu_percent` of `api` in each lane of `status`.
async fn status_cpu(h: &crate::Harness, context: Context) -> Vec<Option<f64>> {
    let value = h
        .call(Method::Status, json!({}), context)
        .await
        .expect("status");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items
        .iter()
        .filter_map(|lane| lane.units.iter().find(|unit| unit.name == "api"))
        .map(|unit| unit.cpu_percent)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
/// A folder's first lane and an extra lane both get sampled, with a history
/// that grows; when the collector stops reading a unit, `metrics` and `status`
/// report no CPU or memory for it, not the last values.
async fn units_are_sampled_and_missing_readings_are_absent() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let source = Arc::new(Switchable {
            reporting: AtomicBool::new(true),
        });
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                metrics: vec![source.clone()],
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::LaneNew, json!({"name": "second"}), cx(&main))
            .await
            .expect("lane new");
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let second = Context {
            lane: Some("second".into()),
            ..cx(&main)
        };
        h.call(Method::Start, json!({}), second)
            .await
            .expect("start the extra lane");

        let sampled = eventually(Duration::from_secs(15), || async {
            let rows = api_rows(&h, cx(&main)).await;
            (rows.len() == 2 && rows.iter().all(|row| row.3 >= 3)).then_some(rows)
        })
        .await
        .expect("both lanes build a history");
        for (lane, cpu, memory, _) in &sampled {
            assert_eq!((cpu, memory), (&json!(3.0), &json!(1 << 20)), "{lane}");
        }
        assert_eq!(sampled[1].0, "demo@1");
        assert_eq!(status_cpu(&h, cx(&main)).await, [Some(3.0), Some(3.0)]);

        source.reporting.store(false, Ordering::SeqCst);
        let rows = eventually(Duration::from_secs(15), || async {
            let rows = api_rows(&h, cx(&main)).await;
            rows.iter()
                .all(|row| row.1.is_null() && row.2.is_null())
                .then_some(rows)
        })
        .await
        .expect("no reading reads as absent");
        assert!(rows.iter().all(|row| row.3 >= 3), "history stays: {rows:?}");
        assert_eq!(status_cpu(&h, cx(&main)).await, [None, None]);
        h.close().await;
    })
    .await
    .expect("timeout");
}
