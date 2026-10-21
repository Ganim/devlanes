//! docker CLI stats math from captured Engine responses, plus a live engine check.
//!
//! The cgroup v2 fixtures were captured from Docker 29.4 (OrbStack) with ids and
//! names replaced. `stats_v1.json` is hand-built in the cgroup v1 shape (no
//! `online_cpus`, `total_inactive_file`) because no v1 host was available.

use devlanes_core::{LaneId, ProjectId};
use devlanes_metrics::{ContainerTarget, MetricsSource, SampleRequest, Subject};
use devlanes_metrics_docker::{
    ContainerStatsResponse, DockerSource, DockerVmInfo, SystemInfo, cpu_percent_unix,
    memory_usage_no_cache, reading,
};

/// Parses a captured stats response.
fn stats(json: &str) -> ContainerStatsResponse {
    serde_json::from_str(json).expect("stats fixture")
}

/// Asserts two percentages agree to within float rounding.
fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn cgroup_v2_with_precpu_matches_docker_cli() {
    let stats = stats(include_str!("../../fixtures/stats_v2_with_precpu.json"));
    let reading = reading(&stats, None).expect("running container");
    // Δtotal 791237029000 - 791223242000, Δsystem 435451310000000 - 435439180000000, 12 CPUs.
    assert_close(
        reading.cpu_percent,
        13_787_000.0 / 12_130_000_000.0 * 12.0 * 100.0,
    );
    assert_eq!(
        reading.memory_bytes,
        12_271_616 - 1_695_744,
        "usage - inactive_file"
    );
    assert_eq!(reading.rss_bytes, 4_530_176, "anon");
    assert_eq!(reading.memory_limit_bytes, 12_600_696_832);
}

#[test]
fn one_shot_without_previous_uses_zero_precpu() {
    let stats = stats(include_str!("../../fixtures/stats_v2_one_shot.json"));
    let first = reading(&stats, None).expect("running container");
    // precpu is zeroed in one-shot mode, so the delta is the full counter.
    assert_close(
        first.cpu_percent,
        658_896_387_000.0 / 435_530_540_000_000.0 * 12.0 * 100.0,
    );
    assert_eq!(first.memory_bytes, 54_194_176 - 2_445_312);

    // The cached previous reading wins over precpu; identical counters give 0%.
    let cpu = stats.cpu_stats.clone().expect("cpu stats");
    let again = reading(&stats, Some(&cpu)).expect("running container");
    assert_eq!(again.cpu_percent, 0.0);
}

#[test]
fn cgroup_v1_uses_total_inactive_file_and_percpu_count() {
    let stats = stats(include_str!("../../fixtures/stats_v1.json"));
    let reading = reading(&stats, None).expect("running container");
    // Δtotal 0.5e9 over Δsystem 4e9 with 4 per-CPU entries.
    assert_close(reading.cpu_percent, 50.0);
    assert_eq!(reading.memory_bytes, 104_857_600 - 20_971_520);
    assert_eq!(reading.rss_bytes, 52_428_800, "total_rss");
}

#[test]
fn edge_cases_follow_docker_cli() {
    let stats = stats(include_str!("../../fixtures/stats_v2_with_precpu.json"));
    let current = stats.cpu_stats.clone().expect("cpu");
    let previous = stats.precpu_stats.clone().expect("precpu");
    // A counter going backwards (container restart) yields 0, not a negative.
    assert_eq!(cpu_percent_unix(&current, &previous), 0.0);

    let mut memory = stats.memory_stats.clone().expect("memory");
    if let Some(map) = memory.stats.as_mut() {
        map.insert("inactive_file".into(), u64::MAX);
    }
    assert_eq!(
        memory_usage_no_cache(&memory),
        12_271_616,
        "inactive >= usage keeps usage"
    );
    memory.stats = None;
    assert_eq!(memory_usage_no_cache(&memory), 12_271_616);

    let stopped = ContainerStatsResponse::default();
    assert!(
        reading(&stopped, None).is_none(),
        "stopped containers have no usage"
    );
}

#[test]
fn docker_info_fixture_gives_vm_capacity() {
    let info: SystemInfo =
        serde_json::from_str(include_str!("../../fixtures/info.json")).expect("info fixture");
    assert_eq!(
        DockerVmInfo::from_info(&info),
        DockerVmInfo {
            name: "orbstack".into(),
            operating_system: "OrbStack".into(),
            server_version: "29.4.0".into(),
            cpus: 12,
            memory_total_bytes: 12_600_696_832,
            containers_running: 4,
        }
    );
    assert_eq!(DockerVmInfo::from_info(&SystemInfo::default()).cpus, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn empty_request_makes_no_engine_calls() {
    // A regular file passes bollard's existence check, but every request fails.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("docker.sock");
    std::fs::write(&path, b"").expect("placeholder");
    let docker = bollard::Docker::connect_with_unix(
        path.to_str().expect("utf-8 path"),
        1,
        bollard::API_DEFAULT_VERSION,
    )
    .expect("client");
    let source = DockerSource::new(docker);
    assert!(
        source
            .sample(&SampleRequest::default())
            .await
            .expect("empty")
            .is_empty()
    );
    let request = SampleRequest {
        units: Vec::new(),
        containers: vec![ContainerTarget {
            project: ProjectId(1),
            lane: None,
            id: "missing".into(),
        }],
    };
    assert!(
        source.sample(&request).await.is_err(),
        "unreachable engine is an error"
    );
}

#[tokio::test]
#[ignore = "needs docker"]
async fn docker_live_stats_and_info() {
    use bollard::query_parameters::ListContainersOptions;

    let source = DockerSource::connect().expect("connect");
    let info = source.vm_info().await.expect("docker info");
    assert!(info.cpus > 0 && info.memory_total_bytes > 0, "{info:?}");

    let docker = bollard::Docker::connect_with_defaults().expect("client");
    let running = docker
        .list_containers(None::<ListContainersOptions>)
        .await
        .expect("list containers");
    let containers: Vec<ContainerTarget> = running
        .iter()
        .filter_map(|summary| summary.id.clone())
        .take(5)
        .map(|id| ContainerTarget {
            project: ProjectId(1),
            lane: Some(LaneId(1)),
            id,
        })
        .collect();
    let request = SampleRequest {
        units: Vec::new(),
        containers: containers.clone(),
    };
    source.sample(&request).await.expect("first sample");
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let samples = source.sample(&request).await.expect("second sample");
    let sampled = samples
        .iter()
        .filter(|sample| matches!(sample.subject, Subject::Container { .. }))
        .count();
    assert_eq!(sampled, containers.len(), "every running container sampled");
    for sample in &samples {
        assert!(sample.cpu_percent >= 0.0 && sample.cpu_percent.is_finite());
        assert!(sample.footprint_bytes > 0, "{sample:?}");
    }
    let vm_rows = samples
        .iter()
        .filter(|sample| sample.subject == Subject::DockerVm)
        .count();
    assert_eq!(
        vm_rows,
        usize::from(cfg!(target_os = "macos") && !containers.is_empty())
    );
}
