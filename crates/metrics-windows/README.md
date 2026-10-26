# devlanes-metrics-windows

Windows process collector and pressure source for `devlanes-metrics`, built on the `devlanes-sys-windows` wrappers. The binary registers `WindowsSource` as a `MetricsSource` and `WindowsPressure` as the start gate's `PressureSource`; the daemon passes in each unit's job members. On other platforms the crate is empty.

## Usage

```rust
use devlanes_core::{LaneId, ProjectId};
use devlanes_metrics::{MetricsSource, SampleRequest, UnitKey, UnitTarget};
use devlanes_metrics_windows::{WindowsSource, process_identity};

async fn sample_self() -> Result<(), Box<dyn std::error::Error>> {
    let source = WindowsSource::new();
    let leader = process_identity(std::process::id()).expect("process is running");
    let request = SampleRequest {
        units: vec![UnitTarget {
            unit: UnitKey { project: ProjectId(1), lane: LaneId(1), unit: "web".into() },
            sessions: Vec::new(),
            processes: vec![leader],
        }],
        ..SampleRequest::default()
    };
    let samples = source.sample(&request).await?; // one Subject::Process sample per member
    println!("{} samples", samples.len());
    Ok(())
}
```

`WindowsSource::sample_blocking` does the same on the calling thread, taking the units and the daemon's identity.

```rust
use devlanes_core::gate::PressureSource;
use devlanes_metrics_windows::WindowsPressure;

async fn pressure() -> Result<(), Box<dyn std::error::Error>> {
    let pressure = WindowsPressure::new();
    let sample = pressure.sample().await?; // cpu_percent is None on the first call
    let available = pressure.available_memory().await?;
    let total = pressure.total_memory().await?;
    println!("{:?} {available:?} {total:?}", sample.cpu_percent);
    Ok(())
}
```

## Behaviour

A unit's processes are exactly the identities the daemon passes in, which on Windows are the members of the unit's job as of the runner's last refresh (every half second). The collector walks no process tree and ignores session ids. A PID whose creation time no longer matches was reused and is skipped, as are exited processes and processes that refuse `PROCESS_QUERY_LIMITED_INFORMATION`. A child that starts and exits between two refreshes is never seen.

When the request names the daemon's own process, it is read by PID alone and reported as one `Subject::Daemon` sample, also when there are no units. It never counts for a unit.

CPU is kernel plus user time from `GetProcessTimes`, as a change since the previous sample. A process seen for the first time reads 0%, unlike macOS and Linux, which report the lifetime average: a process a few milliseconds old would read as several cores because its startup CPU is spread over an age the coarse Windows clocks cannot measure. `footprint_bytes` is the private working set from `PROCESS_MEMORY_COUNTERS_EX2`, the number Task Manager's Memory column shows. `rss_bytes` is the working set, which includes shared DLL pages.

The collector only reads process data and never limits or signals anything.

## Pressure

System CPU is the busy share of all CPUs between two `GetSystemTimes` calls. Windows has no pressure stall information. The memory level is critical while Windows signals low memory (`QueryMemoryResourceNotification` on a `LowMemoryResourceNotification` object, set near exhaustion). It is warn when available physical memory is under both 10% of RAM and 2 GiB, so a large machine still has room for a dev server or two and a small one is judged by its share. The memory load percentage is not used, because Docker Desktop's VM and the file cache keep it above 85% on ordinary dev machines. Available memory is `GlobalMemoryStatusEx`'s available physical memory (free plus standby pages); total memory is its installed physical memory.

## License

Apache-2.0
