# devlanes-metrics-docker

Container collector for `devlanes-metrics`, using the Docker Engine stats API through bollard. The binary registers it as a `MetricsSource`; the daemon passes in the managed containers of each project and lane.

## Usage

```rust
use devlanes_core::{LaneId, ProjectId};
use devlanes_metrics::{ContainerTarget, MetricsSource, SampleRequest};
use devlanes_metrics_docker::DockerSource;

async fn sample_postgres(docker: bollard::Docker) -> Result<(), Box<dyn std::error::Error>> {
    // The binary passes the infra integration's client, so both use one engine.
    let source = DockerSource::new(docker);
    let request = SampleRequest {
        containers: vec![ContainerTarget {
            project: ProjectId(1),
            lane: Some(LaneId(2)),
            id: "acme-lane2-postgres-1".into(),
        }],
        ..SampleRequest::default()
    };
    let samples = source.sample(&request).await?;
    let vm = source.vm_info().await?; // CPUs and memory of the engine host
    println!("{} samples, {vm:?}", samples.len());
    Ok(())
}
```

## API

- `DockerSource`: one `one-shot` stats request per container per tick, run concurrently with a 5 second timeout.
- `cpu_percent_unix`, `memory_usage_no_cache`, `reading`: the docker CLI math on bollard's stats types, usable without an engine.
- `CpuWindow`: a container's recent CPU counters, so CPU is measured over at least `CPU_WINDOW` (2 s).
- `DockerVmInfo`: name, OS, server version, CPUs, total memory and running containers from `docker info`.

## Behaviour

CPU and memory use the math of `docker stats`: they port `calculateCPUPercentUnix` and `calculateMemUsageUnixNoCache` from the docker CLI's [`stats_helpers.go`](https://github.com/docker/cli/blob/master/cli/command/container/stats_helpers.go). CPU is `(container usage delta / system usage delta) * online CPUs * 100`. The deltas span at least 2 s of engine time rather than one tick: a container that works in short bursts about once a second (postgres, RabbitMQ) would otherwise read 0% for every second without a burst. Memory is usage minus `inactive_file` (`total_inactive_file` on cgroup v1).

On macOS containers run inside one VM, so each sample also includes a `Subject::DockerVm` row with the sum of the sampled containers. Missing or stopped containers are skipped. The call fails only when every request fails.

The collector only reads stats and never limits, pauses or updates containers.

## License

Apache-2.0
