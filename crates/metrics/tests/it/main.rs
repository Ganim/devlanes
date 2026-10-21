//! History, rollup, percentile, baseline and aggregation math.

use std::time::Duration;

use devlanes_core::{LaneId, ProjectId, UnitRunId};
use devlanes_metrics::{
    BaselinePolicy, ByteSummary, CpuSummary, History, HistoryConfig, QuantileSketch, Ring,
    RunSummary, RunTracker, Sample, Subject, UnitKey, aggregate, baseline, retention_cutoff_ms,
};

/// Builds a deterministic unit key.
fn unit(name: &str) -> UnitKey {
    UnitKey {
        project: ProjectId(1),
        lane: LaneId(2),
        unit: name.into(),
    }
}

/// Builds a unit-total sample.
fn unit_sample(name: &str, ts_ms: u64, cpu: f64, footprint: u64) -> Sample {
    Sample {
        subject: Subject::Unit(unit(name)),
        cpu_percent: cpu,
        footprint_bytes: footprint,
        rss_bytes: footprint / 2,
        ts_ms,
    }
}

#[test]
fn ring_evicts_oldest_when_full() {
    let mut ring = Ring::new(3);
    assert_eq!(ring.push(1), None);
    assert_eq!(ring.push(2), None);
    assert_eq!(ring.push(3), None);
    assert_eq!(ring.push(4), Some(1));
    assert_eq!(ring.iter().copied().collect::<Vec<_>>(), [2, 3, 4]);
    assert_eq!(
        (ring.first(), ring.last(), ring.len()),
        (Some(&2), Some(&4), 3)
    );
    assert_eq!(Ring::<u8>::new(0).capacity(), 1);
}

#[test]
fn history_rings_evict_by_capacity() {
    let mut history = History::new(HistoryConfig {
        raw_capacity: 5,
        coarse_capacity: 2,
    });
    for second in 0..45 {
        assert!(history.record(&unit_sample("web", second * 1_000, 1.0, 10)));
    }
    let series = history.series(&Subject::Unit(unit("web"))).expect("series");
    let points: Vec<u64> = series.points().map(|p| p.ts_ms).collect();
    assert_eq!(points, [40_000, 41_000, 42_000, 43_000, 44_000]);
    // Windows 0..40 s closed (4 of them); only the newest two survive.
    let windows: Vec<u64> = series.windows().map(|w| w.start_ms).collect();
    assert_eq!(windows, [20_000, 30_000]);
    let open = series.open_window().expect("open window");
    assert_eq!((open.start_ms, open.samples), (40_000, 5));
}

#[test]
fn default_capacities_cover_fifteen_minutes_and_six_hours() {
    let config = HistoryConfig::default();
    assert_eq!(config.raw_capacity as u64 * 1_000, 15 * 60 * 1_000);
    assert_eq!(
        config.coarse_capacity as u64 * devlanes_metrics::COARSE_INTERVAL_MS,
        6 * 3_600 * 1_000
    );
}

#[test]
fn ten_second_windows_aggregate_min_avg_max() {
    let mut history = History::default();
    for (ts, cpu, footprint) in [
        (10_000, 10.0, 100),
        (12_000, 30.0, 300),
        (19_999, 20.0, 200),
    ] {
        history.record(&unit_sample("web", ts, cpu, footprint));
    }
    history.record(&unit_sample("web", 20_000, 0.0, 0));
    let window = *history
        .series(&Subject::Unit(unit("web")))
        .and_then(|s| s.windows().next())
        .expect("closed window");
    assert_eq!((window.start_ms, window.samples), (10_000, 3));
    assert_eq!(
        (window.cpu.min, window.cpu.avg, window.cpu.max),
        (10.0, 20.0, 30.0)
    );
    assert_eq!(
        (
            window.footprint.min,
            window.footprint.avg,
            window.footprint.max
        ),
        (100, 200, 300)
    );
    assert_eq!(
        (window.rss.min, window.rss.avg, window.rss.max),
        (50, 100, 150)
    );
}

#[test]
fn minute_rollups_close_on_new_minute_and_on_drain() {
    let mut history = History::default();
    for second in 0..60 {
        history.record(&unit_sample(
            "web",
            60_000 + second * 1_000,
            second as f64,
            1_000,
        ));
    }
    assert!(
        history.drain_rollups(119_999).is_empty(),
        "minute still open"
    );
    history.record(&unit_sample("web", 120_500, 100.0, 4_000));
    let first = history.drain_rollups(120_500);
    assert_eq!(first.len(), 1);
    let rollup = &first[0];
    assert_eq!(rollup.subject, Subject::Unit(unit("web")));
    assert_eq!(
        (rollup.window.start_ms, rollup.window.samples),
        (60_000, 60)
    );
    assert_eq!(rollup.window.cpu.min, 0.0);
    assert_eq!(rollup.window.cpu.max, 59.0);
    assert!((rollup.window.cpu.avg - 29.5).abs() < 1e-9);
    assert_eq!(rollup.window.footprint.avg, 1_000);

    // An idle series flushes its last minute once that minute has fully elapsed.
    assert!(history.drain_rollups(150_000).is_empty());
    let idle = history.drain_rollups(180_000);
    assert_eq!(idle.len(), 1);
    assert_eq!(
        (idle[0].window.start_ms, idle[0].window.samples),
        (120_000, 1)
    );
    assert!(history.drain_rollups(10_000_000).is_empty(), "drained once");
}

#[test]
fn out_of_order_samples_are_dropped() {
    let mut history = History::default();
    assert!(history.record(&unit_sample("web", 5_000, 1.0, 1)));
    assert!(!history.record(&unit_sample("web", 4_000, 1.0, 1)));
    assert!(history.record(&unit_sample("web", 5_000, 2.0, 1)));
    assert_eq!(
        history
            .series(&Subject::Unit(unit("web")))
            .map(|s| s.points().len()),
        Some(2)
    );
}

#[test]
fn prune_forgets_idle_series_after_drain() {
    let mut history = History::default();
    history.record(&unit_sample("old", 0, 1.0, 1));
    history.record(&unit_sample("new", 3_600_000, 1.0, 1));
    let idle = Duration::from_secs(1_800);
    assert_eq!(
        history.prune(3_600_000, idle),
        0,
        "undrained minute is kept"
    );
    history.drain_rollups(3_600_000);
    assert_eq!(history.prune(3_600_000, idle), 1);
    assert_eq!(history.subjects().count(), 1);
    assert_eq!(retention_cutoff_ms(100, Duration::from_secs(1)), 0);
    assert_eq!(
        retention_cutoff_ms(86_400_000, Duration::from_secs(3_600)),
        82_800_000
    );
}

#[test]
fn sketch_quantiles_stay_within_relative_accuracy() {
    let mut sketch = QuantileSketch::default();
    assert_eq!(sketch.quantile(0.5), None);
    for value in 1..=1_000 {
        sketch.add(f64::from(value));
    }
    let close = |actual: f64, expected: f64| (actual - expected).abs() <= expected * 0.01 + 1e-9;
    let p50 = sketch.quantile(0.5).expect("p50");
    let p95 = sketch.quantile(0.95).expect("p95");
    // Lower ranks: floor(0.5 * 999) = 499 (value 500), floor(0.95 * 999) = 949 (value 950).
    assert!(close(p50, 500.0), "p50 {p50}");
    assert!(close(p95, 950.0), "p95 {p95}");
    assert_eq!(sketch.quantile(0.0), Some(1.0));
    assert_eq!(sketch.quantile(1.0), Some(1_000.0));
    assert_eq!(sketch.max(), Some(1_000.0));

    let mut zeros = QuantileSketch::default();
    for value in [0.0, 0.0, 0.0, 50.0, f64::NAN, -3.0] {
        zeros.add(value);
    }
    assert_eq!(zeros.count(), 5);
    assert_eq!(zeros.quantile(0.5), Some(0.0));
    assert_eq!(zeros.quantile(1.0), Some(50.0));
}

#[test]
fn run_tracker_summarizes_only_its_unit_and_window() {
    let mut tracker = RunTracker::default();
    let run = UnitRunId(7);
    tracker.start(run, unit("web"), 10_000);
    tracker.observe(&unit_sample("web", 9_000, 999.0, 999_999));
    tracker.observe(&unit_sample("api", 11_000, 999.0, 999_999));
    let samples: Vec<Sample> = (1..=100)
        .map(|i| unit_sample("web", 10_000 + i * 1_000, i as f64, i * 1_000))
        .collect();
    tracker.observe_all(&samples);
    let live = tracker.current(run).expect("live");
    assert_eq!((live.samples, live.ended_ms), (100, 110_000));

    let summary = tracker.finish(run, 200_000).expect("summary");
    assert!(tracker.current(run).is_none());
    assert_eq!(summary.samples, 100);
    assert_eq!(summary.duration(), Duration::from_secs(190));
    assert_eq!(summary.footprint.peak, 100_000);
    assert_eq!(summary.cpu.peak, 100.0);
    assert_eq!(summary.rss_peak, 50_000);
    let within = |actual: f64, expected: f64| (actual - expected).abs() <= expected * 0.01;
    assert!(
        within(summary.footprint.p50 as f64, 50_000.0),
        "{:?}",
        summary.footprint
    );
    assert!(
        within(summary.footprint.p95 as f64, 95_000.0),
        "{:?}",
        summary.footprint
    );
    assert!(within(summary.cpu.p95, 95.0), "{:?}", summary.cpu);
    assert!(tracker.finish(UnitRunId(99), 1).is_none());
}

/// Builds a finished run summary with the given p95 values.
fn run(id: i64, ended_ms: u64, duration_s: u64, footprint_p95: u64, cpu_p95: f64) -> RunSummary {
    RunSummary {
        run: UnitRunId(id),
        unit: unit("web"),
        started_ms: ended_ms - duration_s * 1_000,
        ended_ms,
        samples: duration_s,
        footprint: ByteSummary {
            p50: footprint_p95 / 2,
            p95: footprint_p95,
            peak: footprint_p95 * 2,
        },
        cpu: CpuSummary {
            p50: cpu_p95 / 2.0,
            p95: cpu_p95,
            peak: cpu_p95 * 2.0,
        },
        rss_peak: 0,
    }
}

#[test]
fn baseline_is_median_of_recent_qualifying_p95() {
    let policy = BaselinePolicy {
        runs: 3,
        min_duration: Duration::from_secs(120),
    };
    assert_eq!(baseline(&[], policy), None);
    let runs = [
        run(1, 1_000_000, 600, 900, 90.0), // oldest, outside the last 3
        run(2, 2_000_000, 600, 300, 30.0),
        run(3, 3_000_000, 60, 5_000, 500.0), // too short
        run(4, 4_000_000, 600, 100, 10.0),
        run(5, 5_000_000, 600, 200, 20.0),
    ];
    let result = baseline(&runs, policy).expect("baseline");
    assert_eq!(
        (result.footprint_p95, result.cpu_p95, result.runs),
        (200, 20.0, 3)
    );

    let even = BaselinePolicy { runs: 4, ..policy };
    let result = baseline(&runs, even).expect("baseline");
    // p95s 900, 300, 100, 200 -> median of the middle pair (200, 300).
    assert_eq!(
        (result.footprint_p95, result.cpu_p95, result.runs),
        (250, 25.0, 4)
    );

    let defaults = BaselinePolicy::default();
    assert_eq!(
        (defaults.runs, defaults.min_duration),
        (10, Duration::from_secs(120))
    );
}

#[test]
fn aggregate_rolls_processes_and_containers_up() {
    let process = |name: &str, pid: u32, cpu: f64, bytes: u64, ts_ms: u64| Sample {
        subject: Subject::Process {
            unit: unit(name),
            pid,
            start_time: 1,
        },
        cpu_percent: cpu,
        footprint_bytes: bytes,
        rss_bytes: bytes / 2,
        ts_ms,
    };
    let container = |lane: Option<LaneId>, id: &str, bytes: u64| Sample {
        subject: Subject::Container {
            project: ProjectId(1),
            lane,
            id: id.into(),
        },
        cpu_percent: 1.0,
        footprint_bytes: bytes,
        rss_bytes: bytes,
        ts_ms: 1_000,
    };
    let leaves = [
        process("web", 10, 50.0, 100, 1_000),
        process("web", 11, 25.0, 300, 1_200),
        process("api", 12, 5.0, 1_000, 900),
        container(Some(LaneId(2)), "pg-lane", 10_000),
        container(None, "pg-shared", 100_000),
        Sample {
            subject: Subject::DockerVm,
            cpu_percent: 9.0,
            footprint_bytes: 9,
            rss_bytes: 9,
            ts_ms: 1,
        },
    ];
    let totals = aggregate(&leaves);
    let find = |subject: Subject| {
        totals
            .iter()
            .find(|sample| sample.subject == subject)
            .cloned()
            .expect("total")
    };
    let web = find(Subject::Unit(unit("web")));
    assert_eq!(
        (
            web.cpu_percent,
            web.footprint_bytes,
            web.rss_bytes,
            web.ts_ms
        ),
        (75.0, 400, 200, 1_200)
    );
    let lane = find(Subject::Lane {
        project: ProjectId(1),
        lane: LaneId(2),
    });
    assert_eq!((lane.cpu_percent, lane.footprint_bytes), (81.0, 11_400));
    let project = find(Subject::Project {
        project: ProjectId(1),
    });
    assert_eq!(
        (project.cpu_percent, project.footprint_bytes),
        (82.0, 111_400)
    );
    assert_eq!(totals.len(), 4, "web, api, lane, project");
}

#[test]
fn subject_keys_are_stable_and_serde_tagged() {
    let process = Subject::Process {
        unit: unit("a:b"),
        pid: 5,
        start_time: 9,
    };
    assert_eq!(process.key(), "process:1:2:5:9:a:b");
    assert_eq!(Subject::Unit(unit("web")).key(), "unit:1:2:web");
    let shared = Subject::Container {
        project: ProjectId(3),
        lane: None,
        id: "abc".into(),
    };
    assert_eq!(shared.key(), "container:3:-:abc");
    assert_eq!(Subject::DockerVm.key(), "docker_vm");
    let json = serde_json::to_value(Subject::Unit(unit("web"))).expect("json");
    assert_eq!(
        json,
        serde_json::json!({"kind": "unit", "project": 1, "lane": 2, "unit": "web"})
    );
    let back: Subject =
        serde_json::from_value(serde_json::to_value(&process).expect("json")).expect("round trip");
    assert_eq!(back, process);
    assert_eq!(process.unit().map(|u| u.unit.as_str()), Some("a:b"));
    assert_eq!(
        (shared.project(), shared.lane()),
        (Some(ProjectId(3)), None)
    );
}
