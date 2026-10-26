//! Unit metrics: every running unit is sampled, in the extra lanes of a folder
//! too, its history grows with each tick, and a unit that yields no reading
//! shows none instead of its last one. The daemon's own usage is reported on
//! its own.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use devlanes_core::ProcessIdentity;
use devlanes_metrics::{Error, MetricsSource, Sample, SampleRequest, Subject, unix_ms};
use devlanes_proto::{Context, LaneSummary, Method, Page};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, containers::FakeMachine, cx, eventually, harness, repo, temp};

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

/// A `metrics` reply over the last `range_ms`.
async fn metrics(h: &crate::Harness, context: Context, range_ms: u64) -> Value {
    h.call(Method::Metrics, json!({"range_ms": range_ms}), context)
        .await
        .expect("metrics")
}

/// The rows of `metrics[list]` (`units` or `lanes`) under `lane`, of `unit`
/// when given.
fn rows(metrics: &Value, list: &str, lane: &str, unit: Option<&str>) -> Vec<Value> {
    metrics[list]
        .as_array()
        .expect(list)
        .iter()
        .filter(|row| row["lane"] == lane && unit.is_none_or(|unit| row["unit"] == unit))
        .cloned()
        .collect()
}

/// The number of history points of a row.
fn points(row: &Value) -> usize {
    row["history"].as_array().map_or(0, Vec::len)
}

/// Starts `api` in `demo@0` and in the extra lane `second` (`demo@1`) and
/// waits until both have a history.
async fn two_lanes(h: &crate::Harness, main: &std::path::Path) -> Context {
    h.call(Method::LaneNew, json!({"name": "second"}), cx(main))
        .await
        .expect("lane new");
    h.call(Method::Start, json!({}), cx(main))
        .await
        .expect("start");
    let second = Context {
        lane: Some("second".into()),
        ..cx(main)
    };
    h.call(Method::Start, json!({}), second.clone())
        .await
        .expect("start the extra lane");
    let sampled = eventually(Duration::from_secs(15), || async {
        let rows = api_rows(h, cx(main)).await;
        (rows.len() == 2 && rows.iter().all(|row| row.3 >= 2)).then_some(rows)
    })
    .await;
    assert!(sampled.is_some(), "both lanes build a history");
    second
}

#[tokio::test(flavor = "multi_thread")]
/// A stopped lane's `api` row stays as `ended` with no reading and its
/// history and peaks, and its lane row has no reading; a cleaned lane's rows
/// come back `removed` with their history. Both leave once their newest
/// sample is outside the requested range.
async fn stopped_and_cleaned_lanes_keep_their_rows_in_range() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let source = Arc::new(Switchable {
            reporting: AtomicBool::new(true),
        });
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                metrics: vec![source],
                ..Setup::default()
            },
        )
        .await;
        let second = two_lanes(&h, &main).await;
        let live = metrics(&h, cx(&main), 120_000).await;
        for row in rows(&live, "units", "demo@1", Some("api"))
            .iter()
            .chain(&rows(&live, "lanes", "demo@1", None))
        {
            assert_eq!(
                (&row["ended"], &row["removed"]),
                (&json!(false), &json!(false)),
                "{row}"
            );
        }

        h.call(Method::Stop, json!({}), second)
            .await
            .expect("stop the extra lane");
        // The lane row ends with the first tick after the stop.
        let (api, lane) = eventually(Duration::from_secs(15), || async {
            let reply = metrics(&h, cx(&main), 120_000).await;
            let lane = rows(&reply, "lanes", "demo@1", None).pop()?;
            let api = rows(&reply, "units", "demo@1", Some("api")).pop()?;
            (lane["ended"] == true).then_some((api, lane))
        })
        .await
        .expect("the stopped lane's rows stay");
        assert_eq!(
            (&api["ended"], &api["removed"]),
            (&json!(true), &json!(false)),
            "{api}"
        );
        assert!(
            api["cpu_percent"].is_null() && api["memory_bytes"].is_null(),
            "{api}"
        );
        assert!(points(&api) >= 2, "history stays: {api}");
        assert_eq!(
            api["peak_cpu_percent"], 3.0,
            "peaks of the start stay: {api}"
        );
        assert!(
            api["series"]
                .as_str()
                .is_some_and(|s| s.starts_with("unit:"))
        );
        assert!(
            lane["cpu_percent"].is_null() && lane["memory_bytes"].is_null(),
            "no current reading: {lane}"
        );
        assert_eq!(lane["removed"], false, "{lane}");
        assert!(points(&lane) >= 2, "{lane}");

        let yes = Context {
            yes: true,
            ..cx(&main)
        };
        h.call(Method::LaneClean, json!({}), yes)
            .await
            .expect("clean the first lane");
        let cleaned = metrics(&h, cx(&main), 120_000).await;
        let api = rows(&cleaned, "units", "demo@0", Some("api"));
        let lane = rows(&cleaned, "lanes", "demo@0", None);
        assert_eq!((api.len(), lane.len()), (1, 1), "{cleaned}");
        for row in api.iter().chain(&lane) {
            assert_eq!(
                (&row["ended"], &row["removed"]),
                (&json!(true), &json!(true)),
                "{row}"
            );
            assert!(row["cpu_percent"].is_null() && row["peak_cpu_percent"].is_null());
            assert!(points(row) >= 2, "history stays: {row}");
        }
        assert_eq!(
            rows(&cleaned, "units", "demo@1", Some("api"))[0]["removed"],
            false,
            "the stopped lane still exists"
        );

        // Nothing runs, so every newest sample leaves a 1 s range.
        let short = eventually(Duration::from_secs(15), || async {
            let reply = metrics(&h, cx(&main), 1_000).await;
            reply["units"]
                .as_array()
                .is_some_and(Vec::is_empty)
                .then_some(reply)
        })
        .await
        .expect("ended rows leave the range");
        let lanes = short["lanes"].as_array().expect("lanes");
        assert_eq!(lanes.len(), 1, "the removed lane is gone: {short}");
        assert_eq!(
            (&lanes[0]["lane"], &lanes[0]["ended"]),
            (&json!("demo@1"), &json!(true)),
            "an existing lane keeps its row: {short}"
        );
        let wide = metrics(&h, cx(&main), 120_000).await;
        assert_eq!(rows(&wide, "units", "demo@0", Some("api")).len(), 1);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A cleaned lane's reference taken by a new lane gives two rows under it,
/// the removed one and the live one, each with its own series.
async fn a_reused_reference_keeps_the_cleaned_lanes_rows_apart() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let source = Arc::new(Switchable {
            reporting: AtomicBool::new(true),
        });
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                metrics: vec![source],
                ..Setup::default()
            },
        )
        .await;
        two_lanes(&h, &main).await;
        let yes = Context {
            yes: true,
            ..cx(&main)
        };
        h.call(Method::LaneClean, json!({"lane": "demo@1"}), yes)
            .await
            .expect("clean the extra lane");
        h.call(Method::LaneNew, json!({"name": "again"}), cx(&main))
            .await
            .expect("lane new");
        let again = Context {
            lane: Some("again".into()),
            ..cx(&main)
        };
        h.call(Method::Start, json!({}), again)
            .await
            .expect("start the new lane");
        let (api, lanes) = eventually(Duration::from_secs(15), || async {
            let reply = metrics(&h, cx(&main), 120_000).await;
            let api = rows(&reply, "units", "demo@1", Some("api"));
            let lanes = rows(&reply, "lanes", "demo@1", None);
            let fresh = |rows: &[Value]| {
                rows.iter()
                    .any(|row| row["removed"] == false && row["ended"] == false && points(row) >= 1)
            };
            (api.len() == 2 && lanes.len() == 2 && fresh(&api) && fresh(&lanes))
                .then_some((api, lanes))
        })
        .await
        .expect("the new lane is sampled next to the cleaned one");
        for rows in [&api, &lanes] {
            let removed = rows.iter().filter(|row| row["removed"] == true).count();
            assert_eq!(removed, 1, "{rows:?}");
            assert_ne!(rows[0]["series"], rows[1]["series"], "{rows:?}");
            let gone = rows
                .iter()
                .find(|row| row["removed"] == true)
                .expect("removed");
            assert!(gone["cpu_percent"].is_null() && points(gone) >= 2, "{gone}");
            let live = rows
                .iter()
                .find(|row| row["removed"] == false)
                .expect("live");
            assert_eq!(live["cpu_percent"], 3.0, "{live}");
        }
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// Reports each unit's leader at 1 MiB and, when the request names the
/// daemon, the daemon at 2% CPU and 7 MiB. Keeps the daemon identities it was
/// asked about.
#[derive(Default)]
struct WithDaemon {
    asked: Mutex<Vec<Option<ProcessIdentity>>>,
}

#[async_trait]
impl MetricsSource for WithDaemon {
    /// `fake`.
    fn name(&self) -> &'static str {
        "fake"
    }

    /// One leader sample per unit plus the daemon's own sample.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push(request.daemon);
        }
        let ts_ms = unix_ms(std::time::SystemTime::now());
        let reading = |subject: Subject, cpu_percent: f64, bytes: u64| Sample {
            subject,
            cpu_percent,
            footprint_bytes: bytes,
            rss_bytes: bytes,
            ts_ms,
        };
        let mut samples: Vec<Sample> = request
            .units
            .iter()
            .filter_map(|target| {
                let leader = target.processes.first()?;
                let subject = Subject::Process {
                    unit: target.unit.clone(),
                    pid: leader.pid,
                    start_time: leader.start_time,
                };
                Some(reading(subject, 3.0, 1 << 20))
            })
            .collect();
        if request.daemon.is_some() {
            samples.push(reading(Subject::Daemon, 2.0, 7 << 20));
        }
        Ok(samples)
    }
}

#[tokio::test(flavor = "multi_thread")]
/// `metrics` carries the daemon's own process with its PID, readings, peaks
/// and history, kept out of every lane and unit row, and each system history
/// row carries the memory pressure level.
async fn daemon_usage_is_reported_on_its_own() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let source = Arc::new(WithDaemon::default());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                metrics: vec![source.clone()],
                system: Some(Arc::new(FakeMachine)),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");

        let metrics = eventually(Duration::from_secs(15), || async {
            let metrics = h
                .call(Method::Metrics, json!({"windows": true}), cx(&main))
                .await
                .ok()?;
            let lane_sampled = metrics["lanes"]
                .as_array()
                .is_some_and(|lanes| !lanes.is_empty());
            (lane_sampled && metrics["daemon"]["history"].as_array()?.len() >= 2).then_some(metrics)
        })
        .await
        .expect("the daemon builds a history");

        // The engine runs inside the test process.
        let pid = std::process::id();
        let asked = source.asked.lock().expect("asked").clone();
        assert!(
            asked
                .iter()
                .all(|daemon| daemon.is_some_and(|d| d.pid == pid)),
            "{asked:?}"
        );
        let daemon = &metrics["daemon"];
        assert_eq!(daemon["pid"], pid, "{daemon}");
        assert_eq!(
            (&daemon["cpu_percent"], &daemon["memory_bytes"]),
            (&json!(2.0), &json!(7 << 20)),
            "{daemon}"
        );
        assert_eq!(
            (&daemon["peak_cpu_percent"], &daemon["peak_memory_bytes"]),
            (&json!(2.0), &json!(7 << 20)),
            "{daemon}"
        );
        let started = daemon["started_at_ms"].as_u64().expect("started_at_ms");
        assert!(
            started > 0 && started <= unix_ms(std::time::SystemTime::now()),
            "{daemon}"
        );
        assert!(daemon["windows"].is_array(), "{daemon}");
        for point in daemon["history"].as_array().expect("history") {
            assert_eq!(point[2], 7 << 20, "{daemon}");
        }

        for lane in metrics["lanes"].as_array().expect("lanes") {
            assert_eq!(lane["memory_bytes"], 1 << 20, "only api counts: {lane}");
        }
        for unit in metrics["units"].as_array().expect("units") {
            assert_eq!(unit["memory_bytes"], 1 << 20, "{unit}");
        }

        let history = metrics["system"]["history"].as_array().expect("history");
        assert!(!history.is_empty());
        for row in history {
            let row = row.as_array().expect("row");
            assert_eq!(row.len(), 4, "{row:?}");
            assert_eq!(row[3], 1, "warn is level 1: {row:?}");
        }
        h.close().await;
    })
    .await
    .expect("timeout");
}
